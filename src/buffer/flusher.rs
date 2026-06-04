use crate::buffer::frame::{FrameId, FrameState};
use crate::buffer::pool::BufferPool;
use crate::io::FileSystem;
use crate::storage::page::PageId;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{sleep, spawn, JoinHandle};
use std::time::Duration;

/// A pending dirty page ready for batch flush.
///
/// The WAL-before-data ordering decision is made on each frame's `rec_lsn`
/// *before* it is added to the pending set (see [`Flusher::flush_batch`]), so a
/// pending entry only needs the frame id and its disk page id.
#[derive(Debug, Clone, Copy)]
struct Pending {
    fid: FrameId,
    page_id: PageId,
}

/// Default threshold: start flushing when > 10 % of frames are dirty.
pub const DEFAULT_DIRTY_RATIO: f64 = 0.10;

/// Default sleep interval between sweeps.
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 10;

/// Maximum size of a merged write batch (4 MiB).
pub const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Request sent to the flusher thread.
#[derive(Debug)]
pub enum FlushRequest {
    /// Flush every eligible dirty frame now.
    FlushAll,
    /// Stop the flusher loop.
    Shutdown,
}

/// Background thread that continuously writes dirty frames back to disk.
pub struct Flusher {
    /// Handle to the background thread.
    handle: Option<JoinHandle<()>>,
    /// Signal to stop.
    shutdown: Arc<AtomicBool>,
}

impl Flusher {
    /// Start a background flusher thread.
    ///
    /// `pool` and `fs` are captured via `Arc` so the thread can reference
    /// them safely.  `wal_durable_lsn` is the shared atomic updated by
    /// [`WalWriter::flush`] after each successful `sync_data()`: dirty frames
    /// whose `rec_lsn` exceeds this watermark are deferred until the WAL
    /// catches up, enforcing WAL-before-data ordering.
    pub fn new(
        pool: Arc<BufferPool>,
        fs: Arc<dyn FileSystem>,
        dirty_ratio: f64,
        interval_ms: u64,
        wal_durable_lsn: Arc<AtomicU64>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = spawn(move || {
            let interval = Duration::from_millis(interval_ms);
            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    break;
                }

                let dirty_count = pool.iter_frames().filter(|f| {
                    f.desc.state.load(Ordering::Relaxed) == FrameState::Dirty as u8
                }).count();
                let dirty_pct = dirty_count as f64 / pool.frame_count as f64;

                if dirty_pct > dirty_ratio {
                    let candidates = pool.dirty_candidates();
                    if !candidates.is_empty() {
                        // Snapshot the durable LSN once per sweep to give a
                        // consistent ordering boundary for this batch.
                        let durable = wal_durable_lsn.load(Ordering::Acquire);
                        Self::flush_batch(
                            &pool,
                            fs.as_ref(),
                            &candidates,
                            durable,
                        );
                    }
                }

                sleep(interval);
            }
        });

        Self {
            handle: Some(handle),
            shutdown,
        }
    }

    /// Flush a batch of dirty frames using sort-merge elevator batching.
    ///
    /// 1. Gather pending writes, skipping frames whose `rec_lsn` exceeds
    ///    `durable` (WAL-before-data ordering — defer until WAL catches up).
    /// 2. Sort by `page_id` (disk offset).
    /// 3. If a [`DoubleWriteBuffer`] is attached, stage ALL pending pages in a
    ///    single `write_batch()` call before any in-place write is issued.
    /// 4. Group adjacent pages into merged extents up to [`MAX_BATCH_BYTES`].
    /// 5. Write each extent via vectored I/O.
    /// 6. Clear the double-write buffer after all extents succeed.
    /// 7. Update frame descriptors (mark clean, clear rec_lsn).
    fn flush_batch(
        pool: &BufferPool,
        fs: &dyn FileSystem,
        candidates: &[crate::buffer::frame::FrameId],
        durable: u64,
    ) {
        let mut pending: Vec<Pending> = Vec::with_capacity(candidates.len());
        for &fid in candidates {
            let frame = pool.frame(fid);
            let page_id = frame.desc.page_id.load(Ordering::Relaxed);
            if page_id == 0 {
                continue;
            }
            let rec_lsn = frame.desc.rec_lsn.load(Ordering::Relaxed);
            if rec_lsn != u64::MAX && rec_lsn > durable {
                // WAL-before-data ordering: the WAL record for this page has
                // not been flushed to disk yet.  Defer this frame until the
                // WAL's durable LSN advances past rec_lsn.
                continue;
            }
            if frame.desc.io_inflight.swap(true, Ordering::Acquire) {
                continue; // another thread already flushing
            }
            // Re-check the pin after claiming io_inflight: a writer may have
            // pinned the frame since `dirty_candidates()` snapshotted it.  A
            // pinned frame can have a live `PageGuard` writer mutating bytes, so
            // flushing it would race that writer.  Release the claim and skip.
            if frame.desc.is_pinned() {
                frame.desc.io_inflight.store(false, Ordering::Release);
                continue;
            }
            pending.push(Pending { fid, page_id });
        }

        if pending.is_empty() {
            return;
        }

        // Sort by page_id (disk order).
        pending.sort_by_key(|p| p.page_id);

        // Update checksums for all pending frames so the DW copy is consistent.
        //
        // Each frame already has `io_inflight = true` (claimed above), and we
        // take its `io_mutex` for the byte mutation so the update cannot race a
        // concurrent load.  Writers via `PageGuard` wait on `io_inflight`, so
        // no `&mut` from the access path overlaps these references.
        for p in &pending {
            let frame = pool.frame(p.fid);
            let _io_guard = frame.io_mutex.lock();
            // SAFETY: io_inflight is set and io_mutex is held: this is the only
            // live reference to the buffer (see FrameBuf aliasing discipline).
            let buf = unsafe { frame.buf.get_mut() };
            crate::storage::page::SlottedPage::update_checksum_bytes(buf);
        }

        // Stage ALL pending pages in one doublewrite batch before any in-place
        // write.  If we crash between the DW sync and the data writes, recovery
        // can restore any partially-written page from the DW copy.
        if let Some(dw) = pool.doublewrite() {
            // Hold every pending frame's io_mutex for the duration of the
            // staging read so no concurrent load mutates the bytes.
            let _io_guards: Vec<_> =
                pending.iter().map(|p| pool.frame(p.fid).io_mutex.lock()).collect();
            let pages: Vec<(u64, &[u8])> = pending
                .iter()
                .map(|p| {
                    let frame = pool.frame(p.fid);
                    // SAFETY: io_inflight set + io_mutex held: shared read only.
                    let bytes = unsafe { frame.buf.get() };
                    (p.page_id, &bytes[..])
                })
                .collect();
            if let Err(e) = dw.write_batch(&pages, fs) {
                eprintln!("flusher doublewrite staging failed: {}", e);
                // Release inflight locks so frames can be retried.
                for p in &pending {
                    let frame = pool.frame(p.fid);
                    frame.desc.io_inflight.store(false, Ordering::Release);
                }
                return;
            }
        }

        // Track whether all extents succeeded so we know whether to clear DW.
        let mut all_extents_ok = true;

        // Group adjacent pages into extents and write each extent.
        let mut group_start = 0;
        while group_start < pending.len() {
            let mut group_end = group_start + 1;
            let mut group_bytes = crate::storage::page::PAGE_SIZE;
            while group_end < pending.len()
                && pending[group_end].page_id == pending[group_end - 1].page_id + 1
                && group_bytes + crate::storage::page::PAGE_SIZE <= MAX_BATCH_BYTES
            {
                group_bytes += crate::storage::page::PAGE_SIZE;
                group_end += 1;
            }

            let extent = &pending[group_start..group_end];
            if let Err(e) = Self::write_extent(pool, fs, extent) {
                eprintln!("flusher batch write failed for page {}..{}: {}",
                    extent[0].page_id,
                    extent.last().unwrap().page_id,
                    e);
                all_extents_ok = false;
                // Mark frames as no longer inflight so they can be retried.
                for p in extent {
                    let frame = pool.frame(p.fid);
                    frame.desc.io_inflight.store(false, Ordering::Release);
                }
            } else {
                // Update descriptors: mark frame clean.
                for p in extent {
                    let frame = pool.frame(p.fid);
                    frame.desc.dirty.store(false, Ordering::Release);
                    frame.desc.state.store(FrameState::Clean as u8, Ordering::Release);
                    frame.desc.io_inflight.store(false, Ordering::Release);
                    frame.desc.rec_lsn.store(u64::MAX, Ordering::Relaxed);
                }
            }

            group_start = group_end;
        }

        // Clear the double-write buffer only after all in-place writes succeeded.
        // If any extent failed, leave the DW entry so recovery can repair it.
        // Non-fatal: a clear failure just means the DW entry persists until
        // the next successful flush clears it.
        if all_extents_ok {
            let _ = pool.doublewrite().map(|dw| dw.clear(fs));
        }
    }

    /// Write a contiguous extent of frames using vectored I/O.
    fn write_extent(
        pool: &BufferPool,
        fs: &dyn FileSystem,
        extent: &[Pending],
    ) -> std::io::Result<()> {
        use crate::storage::page::{PAGE_SIZE, SlottedPage};

        if extent.is_empty() {
            return Ok(());
        }

        let offset = extent[0].page_id * PAGE_SIZE as u64;
        let handle = fs.open(&pool.data_path, false)?;

        // Hold each frame's io_mutex for the whole vectored write so the bytes
        // cannot be mutated by a concurrent load.  Each frame already has
        // `io_inflight = true`, so writers via `PageGuard` are blocked too.
        let _io_guards: Vec<_> =
            extent.iter().map(|p| pool.frame(p.fid).io_mutex.lock()).collect();

        // First pass: refresh checksums (exclusive byte access under io_mutex).
        for p in extent {
            let frame = pool.frame(p.fid);
            // SAFETY: io_inflight set + io_mutex held: exclusive byte access.
            let buf = unsafe { frame.buf.get_mut() };
            SlottedPage::update_checksum_bytes(buf);
        }

        // Second pass: collect shared slices for the vectored write.
        let mut slices: Vec<&[u8]> = Vec::with_capacity(extent.len());
        for p in extent {
            let frame = pool.frame(p.fid);
            // SAFETY: io_inflight set + io_mutex held: shared read only.
            let bytes = unsafe { frame.buf.get() };
            slices.push(&bytes[..]);
        }

        handle.writev_at(&slices, offset)?;
        handle.sync_data()?;
        Ok(())
    }

    /// Signal the flusher to stop and wait for it.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::pool::BufferPool;
    use crate::io::posix::PosixFileSystem;
    use std::io::Write;

    fn setup(frames: u32) -> (tempfile::TempDir, Arc<PosixFileSystem>, Arc<BufferPool>) {
        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = Arc::new(PosixFileSystem::new(false));
        let file_pages = (frames as u64).max(64);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.set_len(file_pages * crate::storage::page::PAGE_SIZE as u64)
                .unwrap();
            f.flush().unwrap();
        }
        // Initialize all pages with valid checksums.
        let handle = fs.open(&path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * crate::storage::page::PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);
        let pool = Arc::new(BufferPool::new(frames, path));
        (dir, fs, pool)
    }

    /// Helper: create a `wal_durable_lsn` that is already past all LSNs (u64::MAX)
    /// so that WAL-ordering never blocks during tests that do not exercise it.
    fn open_durable_lsn() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(u64::MAX))
    }

    #[test]
    fn flusher_cleans_dirty_frames() {
        let (_dir, fs, pool) = setup(4);
        {
            let mut guard = pool.fix_page(fs.as_ref(), 1).unwrap();
            guard.buf_mut()[0] = 0xDE;
            guard.set_dirty(0);
        }

        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.05, 5, open_durable_lsn());
        // Give the flusher time to run.
        sleep(Duration::from_millis(100));
        drop(flusher);

        let mut found = false;
        for frame in pool.iter_frames() {
            if frame.desc.page_id.load(Ordering::Relaxed) == 1 {
                assert!(!frame.desc.dirty.load(Ordering::Relaxed));
                found = true;
                break;
            }
        }
        assert!(found);
    }

    #[test]
    fn flusher_respects_dirty_ratio_threshold() {
        let (_dir, fs, pool) = setup(100);
        // Dirty only 1 % of frames.
        {
            let mut guard = pool.fix_page(fs.as_ref(), 1).unwrap();
            guard.buf_mut()[0] = 0xAB;
            guard.set_dirty(1);
        }

        // Threshold = 50 %, interval = 5 ms.
        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.50, 5, open_durable_lsn());
        sleep(Duration::from_millis(60));
        drop(flusher);

        // With threshold at 50 % the flusher should NOT have run because
        // only 1/100 frames are dirty.  The frame should still be dirty.
        let mut found = false;
        for frame in pool.iter_frames() {
            if frame.desc.page_id.load(Ordering::Relaxed) == 1 {
                assert!(frame.desc.dirty.load(Ordering::Relaxed));
                found = true;
                break;
            }
        }
        assert!(found);
    }

    #[test]
    fn batch_merge_groups_adjacent_pages() {
        let (_dir, fs, pool) = setup(8);
        // Dirty pages 1 and 2 (adjacent).
        // Use LSN 0 so WAL-before-data filter does not block the flush.
        {
            let mut g1 = pool.fix_page(fs.as_ref(), 1).unwrap();
            g1.buf_mut()[0] = 0xA1;
            g1.set_dirty(0);
            let mut g2 = pool.fix_page(fs.as_ref(), 2).unwrap();
            g2.buf_mut()[0] = 0xA2;
            g2.set_dirty(0);
        }

        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.05, 5, open_durable_lsn());
        sleep(Duration::from_millis(200));
        drop(flusher);

        // Both frames should be clean.
        let mut found1 = false;
        let mut found2 = false;
        for frame in pool.iter_frames() {
            let pid = frame.desc.page_id.load(Ordering::Relaxed);
            let dirty = frame.desc.dirty.load(Ordering::Relaxed);
            let state = frame.desc.state.load(Ordering::Relaxed);
            if pid == 1 {
                assert!(!dirty, "page 1 still dirty, state={}", state);
                found1 = true;
            } else if pid == 2 {
                assert!(!dirty, "page 2 still dirty, state={}", state);
                found2 = true;
            }
        }
        assert!(found1 && found2, "both adjacent pages should be flushed");
    }

    /// Verifies WAL-before-data ordering: a frame with rec_lsn=5 is NOT flushed
    /// while durable_lsn=3, then IS flushed once durable_lsn advances to 5.
    #[test]
    fn wal_before_data_ordering_defers_frame_until_wal_catches_up() {
        let (_dir, fs, pool) = setup(4);

        // Mark page 1 dirty with rec_lsn=5 (WAL record not yet durable).
        {
            let mut guard = pool.fix_page(fs.as_ref(), 1).unwrap();
            guard.buf_mut()[0] = 0xAB;
            guard.set_dirty(5);
        }

        // Start flusher with durable_lsn=3 — should not flush rec_lsn=5.
        let durable = Arc::new(AtomicU64::new(3));
        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.05, 5, durable.clone());

        // Wait long enough for at least 2 sweep cycles.
        sleep(Duration::from_millis(50));

        // Frame must still be dirty: WAL not yet durable.
        let mut still_dirty = false;
        for frame in pool.iter_frames() {
            if frame.desc.page_id.load(Ordering::Relaxed) == 1 {
                still_dirty = frame.desc.dirty.load(Ordering::Relaxed);
                break;
            }
        }
        assert!(still_dirty, "frame must remain dirty while durable_lsn < rec_lsn");

        // Advance durable_lsn to 5 — flusher should now flush the frame.
        durable.store(5, Ordering::Release);

        // Give the flusher time to detect the advance and flush.
        sleep(Duration::from_millis(100));
        drop(flusher);

        let mut flushed = false;
        for frame in pool.iter_frames() {
            if frame.desc.page_id.load(Ordering::Relaxed) == 1 {
                flushed = !frame.desc.dirty.load(Ordering::Relaxed);
                break;
            }
        }
        assert!(flushed, "frame must be clean after durable_lsn >= rec_lsn");
    }
}
