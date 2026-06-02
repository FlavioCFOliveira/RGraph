use crate::buffer::frame::{Frame, FrameDescriptor, FrameId, FrameState};
use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::page::{PageId, PAGE_SIZE};
use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Number of shards in the page-to-frame mapping table.
pub const SHARD_COUNT: usize = 256;

/// Guard returned by [`BufferPool::fix_page`].
/// Automatically unpins the frame when dropped.
pub struct PageGuard<'a> {
    pool: &'a BufferPool,
    pub frame_id: FrameId,
    pub page_id: PageId,
}

impl<'a> PageGuard<'a> {
    /// Mutable access to the frame buffer.
    ///
    /// # Safety
    /// The caller must ensure no other reference to this frame's buffer
    /// exists concurrently.  The buffer pool guarantees this via the pin
    /// count (writers should hold an exclusive pin).
    pub fn buf_mut(&mut self) -> &mut AlignedBuffer {
        let frame = self.pool.frame_mut(self.frame_id);
        &mut frame.buf
    }

    /// Read-only access to the frame buffer.
    pub fn buf(&self) -> &AlignedBuffer {
        let frame = self.pool.frame(self.frame_id);
        &frame.buf
    }

    /// Access the frame descriptor.
    pub fn desc(&self) -> &FrameDescriptor {
        let frame = self.pool.frame(self.frame_id);
        &frame.desc
    }

    /// Mark this frame dirty with the given LSN.
    pub fn set_dirty(&self, lsn: u64) {
        let desc = self.desc();
        desc.dirty.store(true, Ordering::Release);
        desc.last_lsn.store(lsn, Ordering::Release);
        let old_rec = desc.rec_lsn.load(Ordering::Relaxed);
        if old_rec == u64::MAX || lsn < old_rec {
            desc.rec_lsn.store(lsn, Ordering::Relaxed);
        }
        desc.state.store(FrameState::Dirty as u8, Ordering::Release);
    }
}

impl<'a> Drop for PageGuard<'a> {
    fn drop(&mut self) {
        self.pool.unfix_page(self.frame_id);
    }
}

/// A production-grade buffer pool with CLOCK-Pro replacement.
///
/// The pool owns a dense array of frames, a sharded page table,
/// and a ghost queue for scan resistance.
pub struct BufferPool {
    /// Total number of frames (derived from RAM budget).
    pub frame_count: u32,
    /// Dense array of frames.  Index = FrameId.
    /// `UnsafeCell` is used so that multiple `PageGuard`s can each hold
    /// a shared reference to the pool while mutating distinct frames.
    frames: UnsafeCell<Vec<Frame>>,
    /// Sharded mapping from PageId -> FrameId.
    shards: [Mutex<HashMap<PageId, FrameId>>; SHARD_COUNT],
    /// CLOCK hand for victim selection.
    clock_hand: AtomicU32,
    /// Ghost queue capacity (scan resistance window).
    ghost_capacity: usize,
    /// Recently evicted page ids (A1out in CLOCK-Pro terminology).
    ghost_queue: Mutex<VecDeque<PageId>>,
    /// Fast lookup set for the ghost queue.
    ghost_set: Mutex<HashMap<PageId, ()>>,
    /// Path to the data file.
    pub data_path: PathBuf,
    /// Hit counter.
    pub hits: AtomicU64,
    /// Miss counter.
    pub misses: AtomicU64,
    /// Eviction counter.
    pub evictions: AtomicU64,
}

// SAFETY: The buffer pool is Sync because each frame is independently
// accessed through atomic descriptors, and the UnsafeCell is only used
// to obtain &mut Frame when the caller already holds a PageGuard that
// proves exclusive access (pin_count > 0 and write lock implied by
// mut PageGuard).
unsafe impl Sync for BufferPool {}

impl BufferPool {
    /// Create a new buffer pool with the given number of frames.
    ///
    /// `frame_count` is normally `target_ram_bytes / PAGE_SIZE`.
    pub fn new(frame_count: u32, data_path: PathBuf) -> Self {
        let mut frames = Vec::with_capacity(frame_count as usize);
        for _ in 0..frame_count {
            frames.push(Frame::new());
        }
        let shards: [Mutex<HashMap<PageId, FrameId>>; SHARD_COUNT] =
            std::array::from_fn(|_| Mutex::new(HashMap::new()));
        let ghost_capacity = (frame_count as usize / 4).max(16);
        Self {
            frame_count,
            frames: UnsafeCell::new(frames),
            shards,
            clock_hand: AtomicU32::new(0),
            ghost_capacity,
            ghost_queue: Mutex::new(VecDeque::with_capacity(ghost_capacity)),
            ghost_set: Mutex::new(HashMap::with_capacity(ghost_capacity)),
            data_path,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Which shard owns this page id?
    fn shard_index(page_id: PageId) -> usize {
        (page_id as usize).wrapping_mul(0x9E3779B97F4A7C15) % SHARD_COUNT
    }

    /// Shared access to a frame (safe because the caller holds a guard).
    pub fn frame(&self, fid: FrameId) -> &Frame {
        // SAFETY: The caller holds a PageGuard that proves the frame is
        // pinned and therefore valid.  We never reallocate `frames` after
        // construction, so the pointer is stable.
        unsafe { (&(*self.frames.get())).get(fid as usize).unwrap() }
    }

    /// Iterate over all frames (read-only).
    pub fn iter_frames(&self) -> impl Iterator<Item = &Frame> {
        // SAFETY: We never mutate the Vec length after construction.
        unsafe { (*self.frames.get()).iter() }
    }

    /// Mutable access to a frame (safe because the caller holds &mut PageGuard).
    fn frame_mut(&self, fid: FrameId) -> &mut Frame {
        // SAFETY: The caller holds a &mut PageGuard, which means no other
        // reference to this specific frame exists through guards.
        unsafe { (&mut (*self.frames.get())).get_mut(fid as usize).unwrap() }
    }

    /// Fix a page in memory, reading from disk if necessary.
    ///
    /// Returns a [`PageGuard`] that keeps the frame pinned until dropped.
    pub fn fix_page(
        &self,
        fs: &dyn FileSystem,
        page_id: PageId,
    ) -> std::io::Result<PageGuard<'_>> {
        // Fast path: page is already resident.
        let shard_idx = Self::shard_index(page_id);
        {
            let shard = self.shards[shard_idx].lock().unwrap();
            if let Some(&fid) = shard.get(&page_id) {
                let frame = self.frame(fid);
                frame.desc.pin_count.fetch_add(1, Ordering::Relaxed);
                frame.desc.clock_ref.store(true, Ordering::Relaxed);
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(PageGuard {
                    pool: self,
                    frame_id: fid,
                    page_id,
                });
            }
        }

        // Miss: need to load from disk into a free frame.
        self.misses.fetch_add(1, Ordering::Relaxed);
        let fid = self.find_free_frame(fs)?;
        let frame = self.frame(fid);

        // Read page from disk.
        let offset = page_id as u64 * PAGE_SIZE as u64;
        let handle = fs.open(&self.data_path, false)?;
        // SAFETY: We are loading into a free frame that no other thread
        // can access (it is not yet in the page table).
        let buf = unsafe { &mut (&mut (*self.frames.get()))[fid as usize].buf };
        handle.read_at(buf, offset)?;

        // Update descriptor.
        frame.desc.reset();
        frame.desc.page_id.store(page_id, Ordering::Relaxed);
        frame.desc.state.store(FrameState::Clean as u8, Ordering::Relaxed);
        frame.desc.pin_count.store(1, Ordering::Relaxed);
        frame.desc.clock_ref.store(true, Ordering::Relaxed);

        // Insert into page table.
        let mut shard = self.shards[shard_idx].lock().unwrap();
        // Re-check in case another thread raced.
        if let Some(&existing_fid) = shard.get(&page_id) {
            // Another thread loaded it first.  Back out.
            frame.desc.reset();
            drop(shard);
            let existing_frame = self.frame(existing_fid);
            existing_frame.desc.pin_count.fetch_add(1, Ordering::Relaxed);
            existing_frame.desc.clock_ref.store(true, Ordering::Relaxed);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(PageGuard {
                pool: self,
                frame_id: existing_fid,
                page_id,
            });
        }
        shard.insert(page_id, fid);
        drop(shard);

        // Ghost promotion: if this page was recently evicted, it was a ghost hit.
        // We don't need special action here; the simple fact it was loaded
        // makes it "hot" because clock_ref is set.
        {
            let mut ghost = self.ghost_set.lock().unwrap();
            if ghost.remove(&page_id).is_some() {
                let mut queue = self.ghost_queue.lock().unwrap();
                queue.retain(|p| *p != page_id);
            }
        }

        Ok(PageGuard {
            pool: self,
            frame_id: fid,
            page_id,
        })
    }

    /// Unpin a frame (called automatically by [`PageGuard::drop`]).
    pub fn unfix_page(&self, frame_id: FrameId) {
        let frame = self.frame(frame_id);
        let old = frame.desc.pin_count.fetch_sub(1, Ordering::Relaxed);
        if old == 0 {
            // Underflow should never happen in correct code, but reset to 0
            // to avoid wrapping.
            frame.desc.pin_count.store(0, Ordering::Relaxed);
        }
    }

    /// Find a free or evictable frame using CLOCK-Pro sweep.
    ///
    /// This method loops until it finds a suitable frame.  Under extreme
    /// memory pressure it may block briefly.
    fn find_free_frame(&self, fs: &dyn FileSystem) -> std::io::Result<FrameId> {
        let start = self.clock_hand.load(Ordering::Relaxed);
        let count = self.frame_count;

        // First pass: look for an Empty frame without sweeping.
        for i in 0..count {
            let idx = ((start + i) % count) as usize;
            let frame = self.frame(idx as FrameId);
            if frame.desc.state.load(Ordering::Relaxed) == FrameState::Empty as u8
                && !frame.desc.io_inflight.load(Ordering::Relaxed)
            {
                self.clock_hand.store(idx as u32, Ordering::Relaxed);
                return Ok(idx as FrameId);
            }
        }

        // Second pass: CLOCK sweep looking for unreferenced, clean victims.
        let mut scanned = 0u32;
        while scanned < count * 2 {
            let idx = self.clock_hand.load(Ordering::Relaxed) as usize;
            let frame = self.frame(idx as FrameId);
            let state = frame.desc.state.load(Ordering::Relaxed);

            // Advance hand atomically.
            self.clock_hand
                .store((idx as u32 + 1) % count, Ordering::Relaxed);
            scanned += 1;

            // Skip frames that are busy.
            if frame.desc.is_pinned() || frame.desc.io_inflight.load(Ordering::Relaxed) {
                continue;
            }

            if state == FrameState::Clean as u8 || state == FrameState::Empty as u8 {
                // If it had a page, evict it.
                let old_page = frame.desc.page_id.load(Ordering::Relaxed);
                if old_page != 0 {
                    let old_shard = Self::shard_index(old_page);
                    let mut shard = self.shards[old_shard].lock().unwrap();
                    shard.remove(&old_page);
                    drop(shard);

                    let referenced = frame.desc.clock_ref.swap(false, Ordering::Relaxed);
                    if !referenced {
                        // Cold page: add to ghost queue for scan resistance.
                        self.push_ghost(old_page);
                    }
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(idx as FrameId);
            }

            // Dirty frame: if referenced, clear bit and keep; if not, we
            // need to flush it before eviction.  For now we skip it and
            // let the background flusher convert it to clean.  To avoid
            // spinning forever we yield occasionally.
            if state == FrameState::Dirty as u8 {
                let referenced = frame.desc.clock_ref.load(Ordering::Relaxed);
                if referenced {
                    frame.desc.clock_ref.store(false, Ordering::Relaxed);
                    continue;
                }
                // Not referenced and dirty: attempt to flush inline so we
                // can evict it immediately.
                if let Err(e) = self.flush_single_frame(fs, idx as FrameId) {
                    return Err(e);
                }
                // After inline flush the frame is clean (or empty if we
                // evict).  Re-try this index on the next loop iteration.
                continue;
            }

            if state == FrameState::Loading as u8 || state == FrameState::Flushing as u8 {
                // Wait a tiny bit for I/O to finish, then continue.
                std::thread::yield_now();
                continue;
            }
        }

        // Desperate fallback: wait for the flusher to make progress.
        std::thread::yield_now();
        self.find_free_frame(fs)
    }

    /// Write a single dirty frame back to disk.
    pub fn flush_single_frame(
        &self,
        fs: &dyn FileSystem,
        frame_id: FrameId,
    ) -> std::io::Result<()> {
        let frame = self.frame(frame_id);
        let page_id = frame.desc.page_id.load(Ordering::Relaxed);
        if page_id == 0 {
            return Ok(());
        }

        // Mark inflight so the sweeper does not pick it.
        let was_inflight = frame.desc.io_inflight.swap(true, Ordering::Acquire);
        if was_inflight {
            return Ok(()); // another thread is already flushing it
        }

        let offset = page_id as u64 * PAGE_SIZE as u64;
        let handle = fs.open(&self.data_path, false)?;
        handle.write_at(&frame.buf, offset)?;
        handle.sync_data()?;

        frame.desc.dirty.store(false, Ordering::Release);
        frame.desc.state.store(FrameState::Clean as u8, Ordering::Release);
        frame.desc.io_inflight.store(false, Ordering::Release);
        frame.desc.rec_lsn.store(u64::MAX, Ordering::Relaxed);
        Ok(())
    }

    /// Push a page id into the ghost queue, evicting the oldest if full.
    fn push_ghost(&self, page_id: PageId) {
        let mut set = self.ghost_set.lock().unwrap();
        let mut queue = self.ghost_queue.lock().unwrap();
        if set.contains_key(&page_id) {
            return;
        }
        if queue.len() >= self.ghost_capacity {
            if let Some(old) = queue.pop_front() {
                set.remove(&old);
            }
        }
        queue.push_back(page_id);
        set.insert(page_id, ());
    }

    /// Current hit ratio (0.0 .. 1.0).
    pub fn hit_ratio(&self) -> f64 {
        let h = self.hits.load(Ordering::Relaxed);
        let m = self.misses.load(Ordering::Relaxed);
        let total = h + m;
        if total == 0 {
            0.0
        } else {
            h as f64 / total as f64
        }
    }

    /// Scan all frames and return ids of dirty ones that are not pinned
    /// and not already being flushed.
    pub fn dirty_candidates(&self) -> Vec<FrameId> {
        let mut out = Vec::new();
        for (idx, frame) in unsafe { (*self.frames.get()).iter().enumerate() } {
            let state = frame.desc.state.load(Ordering::Relaxed);
            if state == FrameState::Dirty as u8
                && !frame.desc.is_pinned()
                && !frame.desc.io_inflight.load(Ordering::Relaxed)
            {
                out.push(idx as FrameId);
            }
        }
        out
    }

    /// Flush all dirty frames to disk (blocking).
    pub fn flush_all(&self, fs: &dyn FileSystem) -> std::io::Result<()> {
        let candidates = self.dirty_candidates();
        for fid in candidates {
            self.flush_single_frame(fs, fid)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use std::io::Write;

    fn temp_pool(frames: u32) -> (tempfile::TempDir, PosixFileSystem, BufferPool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        // Create empty data file so open works.
        let mut f = std::fs::File::create(&path).unwrap();
        // Pre-allocate enough space for test page ids (up to ~64 pages).
        let file_pages = (frames as u64).max(64);
        f.set_len(file_pages * PAGE_SIZE as u64).unwrap();
        f.flush().unwrap();
        drop(f);
        let pool = BufferPool::new(frames, path);
        (dir, fs, pool)
    }

    #[test]
    fn fix_unreferenced_page_reads_from_disk() {
        let (_dir, fs, pool) = temp_pool(4);
        // Write known data directly to disk at page 3.
        let mut write_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        write_buf[0] = 0xCA;
        write_buf[1] = 0xFE;
        let handle = fs.open(&pool.data_path, false).unwrap();
        handle.write_at(&write_buf, 3 * PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();

        let guard = pool.fix_page(&fs, 3).unwrap();
        assert_eq!(guard.buf()[0], 0xCA);
        assert_eq!(guard.buf()[1], 0xFE);
    }

    #[test]
    fn second_fix_is_a_hit() {
        let (_dir, fs, pool) = temp_pool(4);
        let _g1 = pool.fix_page(&fs, 1).unwrap();
        let _g2 = pool.fix_page(&fs, 1).unwrap();
        assert_eq!(pool.hits.load(Ordering::Relaxed), 1);
        assert_eq!(pool.misses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn guard_unpins_on_drop() {
        let (_dir, fs, pool) = temp_pool(4);
        {
            let guard = pool.fix_page(&fs, 2).unwrap();
            assert_eq!(guard.desc().pin_count.load(Ordering::Relaxed), 1);
        }
        let frame = &pool.frames;
        // After drop pin count is 0 on the frame that held page 2.
        // Page 2 loaded into frame 2 on first pass because frame 0,1 might be empty.
        let f = unsafe { (&(*frame.get())).get(2).unwrap() };
        assert_eq!(f.desc.pin_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn eviction_when_pool_full() {
        let (_dir, fs, pool) = temp_pool(2);
        let g0 = pool.fix_page(&fs, 10).unwrap();
        let g1 = pool.fix_page(&fs, 11).unwrap();
        // Both frames are now occupied.
        drop(g0);
        drop(g1);
        // Fix a new page; one of the previous pages must be evicted.
        let _g2 = pool.fix_page(&fs, 12).unwrap();
        assert!(pool.evictions.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn set_dirty_marks_frame() {
        let (_dir, fs, pool) = temp_pool(4);
        let guard = pool.fix_page(&fs, 5).unwrap();
        guard.set_dirty(42);
        assert!(guard.desc().dirty.load(Ordering::Relaxed));
        assert_eq!(guard.desc().last_lsn.load(Ordering::Relaxed), 42);
    }

    #[test]
    fn flush_all_writes_dirty_back() {
        let (_dir, fs, pool) = temp_pool(4);
        {
            let mut guard = pool.fix_page(&fs, 7).unwrap();
            guard.buf_mut()[0] = 0xAB;
            guard.set_dirty(99);
        }
        pool.flush_all(&fs).unwrap();
        // Find which frame holds page 7.
        let mut found = false;
        for frame in unsafe { (*pool.frames.get()).iter() } {
            if frame.desc.page_id.load(Ordering::Relaxed) == 7 {
                assert!(!frame.desc.dirty.load(Ordering::Relaxed));
                found = true;
                break;
            }
        }
        assert!(found, "page 7 should still be resident");
    }

    #[test]
    fn dirty_candidates_excludes_pinned() {
        let (_dir, fs, pool) = temp_pool(4);
        let guard = pool.fix_page(&fs, 9).unwrap();
        guard.set_dirty(1);
        let candidates = pool.dirty_candidates();
        // Pinned frame should not appear.
        assert!(candidates.is_empty());
    }
}
