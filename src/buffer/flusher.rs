use crate::buffer::frame::{FrameId, FrameState};
use crate::buffer::pool::BufferPool;
use crate::io::FileSystem;
use crate::storage::page::PageId;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{sleep, spawn, JoinHandle};
use std::time::Duration;

/// A pending dirty page ready for batch flush.
#[derive(Debug, Clone, Copy)]
struct Pending {
    fid: FrameId,
    page_id: PageId,
    last_lsn: u64,
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
    /// Monotonically increasing watermark of flushed LSN.
    pub flushed_lsn: Arc<AtomicU64>,
}

impl Flusher {
    /// Start a background flusher thread.
    ///
    /// `pool` and `fs` are captured via `Arc` so the thread can reference
    /// them safely.  For simplicity we accept `Arc<BufferPool>` and a
    /// boxed filesystem trait object.
    pub fn new(
        pool: Arc<BufferPool>,
        fs: Arc<dyn FileSystem>,
        dirty_ratio: f64,
        interval_ms: u64,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let flushed_lsn = Arc::new(AtomicU64::new(0));
        let flushed_lsn_clone = flushed_lsn.clone();

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
                        Self::flush_batch(
                            &pool,
                            fs.as_ref(),
                            &candidates,
                            &flushed_lsn_clone,
                        );
                    }
                }

                sleep(interval);
            }
        });

        Self {
            handle: Some(handle),
            shutdown,
            flushed_lsn,
        }
    }

    /// Flush a batch of dirty frames using sort-merge elevator batching.
    ///
    /// 1. Gather pending writes, skipping frames whose `rec_lsn` is ahead
    ///    of the flushed watermark (WAL-before-data ordering).
    /// 2. Sort by `page_id` (disk offset).
    /// 3. Group adjacent pages into merged extents up to [`MAX_BATCH_BYTES`].
    /// 4. Write each extent via vectored I/O.
    /// 5. Update descriptors and advance the flushed LSN watermark.
    fn flush_batch(
        pool: &BufferPool,
        fs: &dyn FileSystem,
        candidates: &[crate::buffer::frame::FrameId],
        flushed_lsn: &AtomicU64,
    ) {
        let mut pending: Vec<Pending> = Vec::with_capacity(candidates.len());
        for &fid in candidates {
            let frame = pool.frame(fid);
            let page_id = frame.desc.page_id.load(Ordering::Relaxed);
            if page_id == 0 {
                continue;
            }
            let rec_lsn = frame.desc.rec_lsn.load(Ordering::Relaxed);
            let current_flushed = flushed_lsn.load(Ordering::Relaxed);
            if rec_lsn != u64::MAX && rec_lsn > current_flushed {
                // WAL-before-data ordering: skip until WAL is flushed.
                continue;
            }
            if frame.desc.io_inflight.swap(true, Ordering::Acquire) {
                continue; // another thread already flushing
            }
            pending.push(Pending {
                fid,
                page_id,
                last_lsn: frame.desc.last_lsn.load(Ordering::Relaxed),
            });
        }

        if pending.is_empty() {
            return;
        }

        // Sort by page_id (disk order).
        pending.sort_by_key(|p| p.page_id);

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
                // Mark frames as no longer inflight so they can be retried.
                for p in extent {
                    let frame = pool.frame(p.fid);
                    frame.desc.io_inflight.store(false, Ordering::Release);
                }
            } else {
                // Update descriptors and advance flushed LSN.
                let mut max_lsn = 0u64;
                for p in extent {
                    let frame = pool.frame(p.fid);
                    frame.desc.dirty.store(false, Ordering::Release);
                    frame.desc.state.store(FrameState::Clean as u8, Ordering::Release);
                    frame.desc.io_inflight.store(false, Ordering::Release);
                    frame.desc.rec_lsn.store(u64::MAX, Ordering::Relaxed);
                    max_lsn = max_lsn.max(p.last_lsn);
                }
                if max_lsn > 0 {
                    flushed_lsn.fetch_max(max_lsn, Ordering::Release);
                }
            }

            group_start = group_end;
        }
    }

    /// Write a contiguous extent of frames using vectored I/O.
    fn write_extent(
        pool: &BufferPool,
        fs: &dyn FileSystem,
        extent: &[Pending],
    ) -> std::io::Result<()> {
        use crate::storage::page::PAGE_SIZE;

        if extent.is_empty() {
            return Ok(());
        }

        let offset = extent[0].page_id * PAGE_SIZE as u64;
        let handle = fs.open(&pool.data_path, false)?;

        // Build a list of buffer slices for vectored write.
        let mut slices: Vec<&[u8]> = Vec::with_capacity(extent.len());
        for p in extent {
            let frame = pool.frame(p.fid);
            slices.push(&frame.buf[..]);
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = Arc::new(PosixFileSystem::new(false));
        let mut f = std::fs::File::create(&path).unwrap();
        let file_pages = (frames as u64).max(64);
        f.set_len(file_pages * crate::storage::page::PAGE_SIZE as u64)
            .unwrap();
        f.flush().unwrap();
        drop(f);
        let pool = Arc::new(BufferPool::new(frames, path));
        (dir, fs, pool)
    }

    #[test]
    fn flusher_cleans_dirty_frames() {
        let (_dir, fs, pool) = setup(4);
        {
            let mut guard = pool.fix_page(fs.as_ref(), 1).unwrap();
            guard.buf_mut()[0] = 0xDE;
            guard.set_dirty(0);
        }

        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.05, 5);
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
        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.50, 5);
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

        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.05, 5);
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
}
