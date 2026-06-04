use crate::buffer::frame::{Frame, FrameDescriptor, FrameId, FrameState, INVALID_FRAME_ID};
use crate::buffer::NumaTopology;
use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::page::{PageId, PAGE_SIZE};
use crate::wal::doublewrite::DoubleWriteBuffer;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Number of shards in the page-to-frame mapping table.
pub const SHARD_COUNT: usize = 256;

/// Sentinel `FrameId` stored in the shard map while a page is being loaded.
///
/// The miss path inserts this value under the shard lock *before* it releases
/// the lock to perform disk I/O.  Any racing thread that observes the sentinel
/// must spin-yield (via [`BufferPool::wait_for_loading`]) until a real
/// `FrameId` replaces it.  This guarantees that:
///
/// * at most one thread issues I/O for a given page, and
/// * no thread can pin or read a frame before its first load has completed.
const LOADING_SENTINEL: FrameId = INVALID_FRAME_ID - 1;

/// Guard returned by [`BufferPool::fix_page`].
/// Automatically unpins the frame when dropped.
pub struct PageGuard<'a> {
    pool: &'a BufferPool,
    pub frame_id: FrameId,
    pub page_id: PageId,
}

impl std::fmt::Debug for PageGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageGuard")
            .field("frame_id", &self.frame_id)
            .field("page_id", &self.page_id)
            .finish()
    }
}

impl<'a> PageGuard<'a> {
    /// Mutable access to the frame buffer.
    ///
    /// Sound because:
    ///
    /// * `&mut self` is the unique live reference to this guard, so no other
    ///   `&mut`/`&` can be derived from *this* guard.
    /// * `pin_count > 0` is held for the guard's lifetime, and every flush
    ///   path skips pinned frames before claiming `io_inflight`
    ///   ([`BufferPool::flush_single_frame`], [`BufferPool::find_free_frame`],
    ///   [`BufferPool::dirty_candidates`]).  Therefore no flusher can be reading
    ///   these bytes while the pin is held.
    /// * The `io_mutex` acquisition below establishes a happens-before edge
    ///   with any flush that finished just before this guard pinned the frame,
    ///   so the `&mut` never aliases a stale flush read.
    pub fn buf_mut(&mut self) -> &mut AlignedBuffer {
        let frame = self.pool.frame(self.frame_id);
        // Synchronise with any in-flight / just-finished flush, then proceed.
        frame.wait_io_quiescent();
        // SAFETY: pin held + frame quiescent: this is the only live reference.
        unsafe { frame.buf.get_mut() }
    }

    /// Read-only access to the frame buffer.
    pub fn buf(&self) -> &AlignedBuffer {
        let frame = self.pool.frame(self.frame_id);
        // SAFETY: a live PageGuard pins the frame; no writer can hold an
        // overlapping &mut while this shared borrow exists (buf_mut needs
        // &mut self).
        unsafe { frame.buf.get() }
    }

    /// Access the frame descriptor.
    pub fn desc(&self) -> &FrameDescriptor {
        &self.pool.frame(self.frame_id).desc
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

    /// Zero-copy read-only slice of the frame buffer.
    ///
    /// The returned `&[u8]` points directly into the pinned frame — no
    /// intermediate copy is performed.  The underlying frame cannot be
    /// evicted while this guard (and therefore the slice) is alive.
    pub fn as_slice(&self) -> &[u8] {
        let frame = self.pool.frame(self.frame_id);
        // SAFETY: same shared-borrow argument as `buf`.
        unsafe { &frame.buf.get()[..] }
    }

    /// Zero-copy mutable slice of the frame buffer.
    ///
    /// Sound for the same reason as [`PageGuard::buf_mut`].
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        let frame = self.pool.frame(self.frame_id);
        frame.wait_io_quiescent();
        // SAFETY: same exclusive-borrow argument as `buf_mut`.
        unsafe { &mut frame.buf.get_mut()[..] }
    }
}

impl Drop for PageGuard<'_> {
    fn drop(&mut self) {
        self.pool.unfix_page(self.frame_id);
    }
}

/// Alias for [`PageGuard`] used by the query engine and storage layers.
///
/// The name `PageHandle` emphasises that the guard is a *handle* to a
/// pinned frame that prevents eviction and exposes zero-copy slices.
pub type PageHandle<'a> = PageGuard<'a>;

/// A production-grade buffer pool with CLOCK-Pro replacement.
///
/// # Concurrency model
///
/// - **Shard locks** (one per 256 pages) guard the page-table entries.
/// - **`LOADING_SENTINEL`**: the miss path inserts this sentinel *under the
///   shard lock* before releasing the lock to perform I/O.  Racing threads
///   that see the sentinel spin-yield until the real `FrameId` is installed,
///   preventing duplicate I/O and premature publication of a frame.
/// - **Per-frame `io_mutex`**: serialises background flush and load-from-disk
///   for a single frame's bytes.
/// - **`FrameDescriptor` atomics**: allow lock-free inspection of metadata
///   (pin count, dirty flag, state, LSNs) by the flusher and CLOCK sweeper.
/// - **`pin_count`**: a frame with `pin_count > 0` is exempt from eviction.
///   The [`PageGuard`] RAII type increments on creation and decrements on drop.
///
/// # Page-id 0
///
/// [`BufferPool::fix_page`] rejects page id `0` with [`io::ErrorKind::InvalidInput`].
/// Page 0 is the null/superblock sentinel and must never alias a data frame;
/// `0` is also the value [`FrameDescriptor::reset`] writes to mark a frame empty.
#[derive(Debug)]
pub struct BufferPool {
    /// Total number of frames (derived from RAM budget).
    pub frame_count: u32,
    /// Dense array of frames.  Index == FrameId.  Never reallocated after
    /// construction, so `&Frame` borrows stay valid for the pool's lifetime.
    /// Per-frame interior mutability lives inside `Frame`'s `FrameBuf`.
    frames: Vec<Frame>,
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
    /// Optional double-write buffer for torn-page protection.
    ///
    /// When set, every single-frame flush stages the page through the
    /// double-write buffer before the in-place write so that partial writes
    /// can be detected and repaired on restart.
    doublewrite: Option<Arc<DoubleWriteBuffer>>,
}

// SAFETY: `BufferPool` is `Send + Sync` because:
//  1. Every mutable access to a frame's buffer is mediated through either the
//     per-frame `io_mutex` (I/O path) or the `PageGuard` pin invariant
//     (`pin_count > 0` + `&mut PageGuard`, normal access path).  Both prevent
//     overlapping `&mut` references to the same bytes (see `FrameBuf`).
//  2. `frames: Vec<Frame>` is never reallocated after construction, so the
//     `&Frame` references handed out by `frame()`/`iter_frames()` remain valid.
//  3. All shared counters and descriptor fields are atomics.
//  4. Shard maps, ghost structures, and the doublewrite handle are protected
//     by `std::sync::Mutex` / `Arc` respectively.
unsafe impl Sync for BufferPool {}
unsafe impl Send for BufferPool {}

impl BufferPool {
    /// Create a new buffer pool with the given number of frames.
    ///
    /// `frame_count` is normally `target_ram_bytes / PAGE_SIZE`.
    pub fn new(frame_count: u32, data_path: PathBuf) -> Self {
        Self::new_numa(frame_count, data_path, &NumaTopology::detect())
    }

    /// Create a new buffer pool with explicit NUMA topology.
    ///
    /// Frames are partitioned evenly across NUMA nodes and allocated with
    /// `mbind(MPOL_BIND)` on Linux.  On non-NUMA systems this behaves
    /// identically to [`Self::new`].
    pub fn new_numa(frame_count: u32, data_path: PathBuf, topo: &NumaTopology) -> Self {
        let mut frames = Vec::with_capacity(frame_count as usize);
        if topo.is_numa() {
            // Partition frames across nodes round-robin.
            for i in 0..frame_count {
                let node_id = (i as usize) % topo.node_count;
                let buf = AlignedBuffer::zeroed_on_node(PAGE_SIZE, Some(node_id));
                frames.push(Frame::with_buffer(buf));
            }
        } else {
            for _ in 0..frame_count {
                frames.push(Frame::new());
            }
        }
        let shards: [Mutex<HashMap<PageId, FrameId>>; SHARD_COUNT] =
            std::array::from_fn(|_| Mutex::new(HashMap::new()));
        let ghost_capacity = (frame_count as usize / 4).max(16);
        Self {
            frame_count,
            frames,
            shards,
            clock_hand: AtomicU32::new(0),
            ghost_capacity,
            ghost_queue: Mutex::new(VecDeque::with_capacity(ghost_capacity)),
            ghost_set: Mutex::new(HashMap::with_capacity(ghost_capacity)),
            data_path,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            doublewrite: None,
        }
    }

    /// Which shard owns this page id?
    fn shard_index(page_id: PageId) -> usize {
        (page_id as usize).wrapping_mul(0x9E3779B97F4A7C15) % SHARD_COUNT
    }

    /// Attach a [`DoubleWriteBuffer`] to this pool.
    ///
    /// Once set, every [`flush_single_frame`] call stages the page through the
    /// double-write buffer before the final in-place write, providing torn-page
    /// protection.
    ///
    /// [`flush_single_frame`]: BufferPool::flush_single_frame
    pub fn set_doublewrite(&mut self, dw: Arc<DoubleWriteBuffer>) {
        self.doublewrite = Some(dw);
    }

    /// Return the attached double-write buffer, if any.
    pub fn doublewrite(&self) -> Option<&Arc<DoubleWriteBuffer>> {
        self.doublewrite.as_ref()
    }

    /// Shared access to a frame.
    ///
    /// Returns `&Frame` directly: the frame array is never reallocated and the
    /// frame's metadata is atomic, so a shared borrow is always sound.  Buffer
    /// bytes inside the returned frame must still be accessed under the
    /// `FrameBuf` aliasing discipline (held pin or `io_mutex`).
    pub fn frame(&self, fid: FrameId) -> &Frame {
        &self.frames[fid as usize]
    }

    /// Iterate over all frames (read-only metadata access).
    pub fn iter_frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter()
    }

    /// Fix a page in memory, reading from disk if necessary.
    ///
    /// Returns a [`PageGuard`] that keeps the frame pinned until dropped.
    ///
    /// # Errors
    ///
    /// - [`io::ErrorKind::InvalidInput`] if `page_id == 0` (the null sentinel).
    /// - [`io::ErrorKind::InvalidData`] if the loaded page fails checksum
    ///   verification.
    /// - Propagates I/O errors from the underlying read.
    pub fn fix_page(&self, fs: &dyn FileSystem, page_id: PageId) -> std::io::Result<PageGuard<'_>> {
        if page_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page_id 0 is reserved; cannot fix the null-sentinel page",
            ));
        }

        let shard_idx = Self::shard_index(page_id);

        // ------------------------------------------------------------------
        // Fast path: page is already resident.
        // ------------------------------------------------------------------
        {
            let shard = self.shards[shard_idx].lock().unwrap();
            match shard.get(&page_id).copied() {
                Some(fid) if fid == LOADING_SENTINEL => {
                    drop(shard);
                    return self.wait_for_loading(fs, page_id);
                }
                Some(fid) => {
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
                None => {}
            }
        }

        // ------------------------------------------------------------------
        // Miss path — two-phase load.
        //
        // Phase 1: reserve a free frame and publish LOADING_SENTINEL under the
        //          shard lock so racing threads wait instead of issuing
        //          duplicate I/O or touching a half-loaded frame.
        // Phase 2: perform I/O with `io_mutex` held; publish the real FrameId.
        // ------------------------------------------------------------------
        self.misses.fetch_add(1, Ordering::Relaxed);
        let fid = self.find_free_frame(fs)?;

        // Phase 1: publish the sentinel under the shard lock.
        {
            let mut shard = self.shards[shard_idx].lock().unwrap();
            match shard.get(&page_id).copied() {
                Some(existing) if existing == LOADING_SENTINEL => {
                    // Another thread is already loading; abandon our frame.
                    self.frame(fid).desc.reset();
                    drop(shard);
                    return self.wait_for_loading(fs, page_id);
                }
                Some(existing) => {
                    // Another thread completed the load while we searched for a
                    // free frame.  Back out and pin the existing frame.
                    self.frame(fid).desc.reset();
                    let frame = self.frame(existing);
                    frame.desc.pin_count.fetch_add(1, Ordering::Relaxed);
                    frame.desc.clock_ref.store(true, Ordering::Relaxed);
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(PageGuard {
                        pool: self,
                        frame_id: existing,
                        page_id,
                    });
                }
                None => {
                    // We are first: reserve the slot with the sentinel.
                    shard.insert(page_id, LOADING_SENTINEL);
                    // Pre-configure the frame so the eviction sweep skips it.
                    let frame = self.frame(fid);
                    frame.desc.reset();
                    frame
                        .desc
                        .state
                        .store(FrameState::Loading as u8, Ordering::Relaxed);
                    frame.desc.page_id.store(page_id, Ordering::Relaxed);
                    frame.desc.io_inflight.store(true, Ordering::Relaxed);
                }
            }
        }

        // Phase 2: perform I/O.  `io_mutex` serialises against any background
        // flush; the LOADING_SENTINEL keeps every other path away from this
        // frame's bytes until we publish the real FrameId below.
        let offset = page_id * PAGE_SIZE as u64;
        let io_result: std::io::Result<()> = (|| {
            let frame = self.frame(fid);
            let _io_guard = frame.io_mutex.lock();
            let handle = fs.open(&self.data_path, false)?;
            // SAFETY: io_mutex is held and the LOADING_SENTINEL prevents any
            // other thread from forming a reference to this buffer.
            let buf = unsafe { frame.buf.get_mut() };
            handle.read_at(buf, offset)?;

            // Verify the checksum before the page is published.
            // SAFETY: still under io_mutex; exclusive access to the bytes.
            let bytes = unsafe { frame.buf.get() };
            if !crate::storage::page::SlottedPage::verify_checksum_bytes(bytes) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("page {} checksum mismatch", page_id),
                ));
            }
            Ok(())
        })();

        // Publish the real FrameId (or remove the sentinel on error).
        {
            let mut shard = self.shards[shard_idx].lock().unwrap();
            let frame = self.frame(fid);
            if io_result.is_ok() {
                frame.desc.io_inflight.store(false, Ordering::Release);
                frame.desc.state.store(FrameState::Clean as u8, Ordering::Release);
                frame.desc.pin_count.store(1, Ordering::Release);
                frame.desc.clock_ref.store(true, Ordering::Release);
                shard.insert(page_id, fid);
            } else {
                frame.desc.reset();
                shard.remove(&page_id);
            }
        }

        io_result?;

        // Ghost promotion: if this page was recently evicted, drop it from the
        // ghost queue.  Being loaded already makes it "hot" (clock_ref set).
        {
            let mut ghost = self.ghost_set.lock().unwrap();
            if ghost.remove(&page_id).is_some() {
                let mut queue = self.ghost_queue.lock().unwrap();
                queue.retain(|p| *p != page_id);
            }
        }

        // Sequential readahead: if the per-thread tracker signals a prefetch,
        // load the next batch of pages into free frames.
        if let Some(prefetch_start) = crate::buffer::readahead::track_access(page_id) {
            let _ = self.prefetch_range(
                fs,
                prefetch_start,
                crate::buffer::readahead::MAX_PREFETCH_PAGES as u32,
            );
        }

        Ok(PageGuard {
            pool: self,
            frame_id: fid,
            page_id,
        })
    }

    /// Spin-yield until the [`LOADING_SENTINEL`] for `page_id` is replaced with
    /// a real `FrameId`, then pin and return a guard for it.
    ///
    /// If the in-flight load fails (the sentinel is removed without a real
    /// FrameId being installed), this retries the whole [`fix_page`] flow.
    ///
    /// [`fix_page`]: BufferPool::fix_page
    fn wait_for_loading(
        &self,
        fs: &dyn FileSystem,
        page_id: PageId,
    ) -> std::io::Result<PageGuard<'_>> {
        let shard_idx = Self::shard_index(page_id);
        loop {
            std::thread::yield_now();
            let shard = self.shards[shard_idx].lock().unwrap();
            match shard.get(&page_id).copied() {
                Some(fid) if fid == LOADING_SENTINEL => {
                    // Still loading; release the lock and keep spinning.
                    drop(shard);
                    continue;
                }
                Some(fid) => {
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
                None => {
                    // The load failed and removed the sentinel; retry from the
                    // top so this thread issues its own load.
                    drop(shard);
                    return self.fix_page(fs, page_id);
                }
            }
        }
    }

    /// Prefetch a contiguous range of pages into free frames.
    ///
    /// This is best-effort: if free frames run out, a page is already resident,
    /// or a page fails its checksum, the method simply skips it.  Each frame's
    /// buffer is filled under its `io_mutex`; the frame is published to the
    /// shard map only after a successful read and checksum check.
    pub fn prefetch_range(
        &self,
        fs: &dyn FileSystem,
        start_page: PageId,
        count: u32,
    ) -> std::io::Result<()> {
        // Cap the reservation so prefetch never starves real fixes: take at
        // most half the pool (and never the whole thing).  This also bounds the
        // number of frames whose `io_mutex` we will hold during the read.
        let budget = (self.frame_count / 2).max(1).min(count);

        // Reserve free frames first (without taking the io_mutex), tracking
        // which page each will hold.  We use the *non-blocking* sweep so a
        // saturated pool simply yields fewer prefetched pages instead of
        // blocking (or, worse, recursing) on `find_free_frame`.
        let mut frames_to_fill: Vec<(FrameId, PageId)> = Vec::with_capacity(budget as usize);
        for i in 0..count {
            if frames_to_fill.len() as u32 >= budget {
                break;
            }
            let page_id = start_page + i as u64;
            if page_id == 0 {
                continue; // page 0 is the null sentinel; never cache it
            }
            let shard_idx = Self::shard_index(page_id);
            {
                let shard = self.shards[shard_idx].lock().unwrap();
                if shard.contains_key(&page_id) {
                    continue; // already resident (or loading)
                }
            }
            match self.try_find_free_frame(fs)? {
                Some(fid) => {
                    let frame = self.frame(fid);
                    frame.desc.reset();
                    frame
                        .desc
                        .state
                        .store(FrameState::Loading as u8, Ordering::Relaxed);
                    frame.desc.page_id.store(page_id, Ordering::Relaxed);
                    frame.desc.io_inflight.store(true, Ordering::Relaxed);
                    frames_to_fill.push((fid, page_id));
                }
                None => break, // no free frames immediately available
            }
        }

        if frames_to_fill.is_empty() {
            return Ok(());
        }

        // Issue a single vectored read into the reserved buffers, all held
        // under their respective io_mutexes for the duration of the read.
        let offset = start_page * PAGE_SIZE as u64;
        let handle = fs.open(&self.data_path, false)?;
        {
            // Collect io_mutex guards so no background flush can touch these
            // buffers while the vectored read is in flight.
            let guards: Vec<_> = frames_to_fill
                .iter()
                .map(|(fid, _)| self.frame(*fid).io_mutex.lock())
                .collect();

            let mut bufs: Vec<&mut [u8]> = Vec::with_capacity(frames_to_fill.len());
            for (fid, _) in &frames_to_fill {
                let frame = self.frame(*fid);
                // SAFETY: this frame's io_mutex is held in `guards`, and it has
                // not been published to the shard map yet, so no other thread
                // can reach these bytes.
                let buf = unsafe { frame.buf.get_mut() };
                bufs.push(&mut buf[..]);
            }
            handle.readv_at(&mut bufs, offset)?;
            drop(guards);
        }

        // Verify checksums and publish each frame.
        for (fid, page_id) in &frames_to_fill {
            let frame = self.frame(*fid);
            // SAFETY: the frame is not yet published; we are the only accessor.
            let valid = {
                let _io_guard = frame.io_mutex.lock();
                let bytes = unsafe { frame.buf.get() };
                crate::storage::page::SlottedPage::verify_checksum_bytes(bytes)
            };
            if !valid {
                // Corrupt page: discard the frame, do not publish it.
                frame.desc.reset();
                continue;
            }
            frame.desc.io_inflight.store(false, Ordering::Release);
            frame.desc.state.store(FrameState::Clean as u8, Ordering::Release);
            frame.desc.pin_count.store(0, Ordering::Relaxed);
            frame.desc.clock_ref.store(false, Ordering::Relaxed);

            let shard_idx = Self::shard_index(*page_id);
            let mut shard = self.shards[shard_idx].lock().unwrap();
            // Only publish if nobody raced us in for this page.
            if let std::collections::hash_map::Entry::Vacant(e) = shard.entry(*page_id) {
                e.insert(*fid);
            } else {
                // Lost the race: drop our copy so it can be reused.
                drop(shard);
                frame.desc.reset();
            }
        }

        Ok(())
    }

    /// Unpin a frame (called automatically by [`PageGuard::drop`]).
    pub fn unfix_page(&self, frame_id: FrameId) {
        let frame = self.frame(frame_id);
        let old = frame.desc.pin_count.fetch_sub(1, Ordering::Relaxed);
        if old == 0 {
            // Underflow guard: clamp back to 0 rather than wrapping to u16::MAX.
            frame.desc.pin_count.store(0, Ordering::Relaxed);
        }
    }

    /// Find a free or evictable frame using a CLOCK-Pro sweep.
    ///
    /// This blocks until it finds a suitable frame: under memory pressure it
    /// yields between sweeps to let the flusher convert dirty frames to clean.
    /// The loop is bounded per attempt (no unbounded recursion), so a pool that
    /// is momentarily saturated by in-flight I/O simply spins-yields rather
    /// than overflowing the stack.
    fn find_free_frame(&self, fs: &dyn FileSystem) -> std::io::Result<FrameId> {
        loop {
            if let Some(fid) = self.sweep_for_victim(fs)? {
                return Ok(fid);
            }
            // No victim this sweep (everything pinned / in-flight); yield and
            // retry so the flusher and concurrent unpins can make progress.
            std::thread::yield_now();
        }
    }

    /// Try to find a free or evictable frame in a single bounded sweep.
    ///
    /// Returns `Ok(None)` when no frame is immediately reclaimable (all frames
    /// pinned or with I/O in flight).  Callers that must not block — such as
    /// best-effort prefetch — use this and stop when it returns `None`.
    fn try_find_free_frame(&self, fs: &dyn FileSystem) -> std::io::Result<Option<FrameId>> {
        self.sweep_for_victim(fs)
    }

    /// One CLOCK-Pro sweep: returns a reclaimed `FrameId`, or `None` if this
    /// pass found no evictable frame.  Inline-flushes one unreferenced dirty
    /// victim per encounter so repeated calls drain dirty frames.
    fn sweep_for_victim(&self, fs: &dyn FileSystem) -> std::io::Result<Option<FrameId>> {
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
                return Ok(Some(idx as FrameId));
            }
        }

        // Second pass: CLOCK sweep for unreferenced, clean victims.
        let mut scanned = 0u32;
        while scanned < count * 2 {
            let idx = self.clock_hand.load(Ordering::Relaxed) as usize;
            let frame = self.frame(idx as FrameId);
            let state = frame.desc.state.load(Ordering::Relaxed);

            // Advance the hand.
            self.clock_hand
                .store((idx as u32 + 1) % count, Ordering::Relaxed);
            scanned += 1;

            // Skip busy frames (pinned or with I/O in flight, which also covers
            // frames reserved by a concurrent two-phase load).
            if frame.desc.is_pinned() || frame.desc.io_inflight.load(Ordering::Relaxed) {
                continue;
            }

            if state == FrameState::Clean as u8 || state == FrameState::Empty as u8 {
                let old_page = frame.desc.page_id.load(Ordering::Relaxed);
                if old_page != 0 {
                    let old_shard = Self::shard_index(old_page);
                    let mut shard = self.shards[old_shard].lock().unwrap();
                    // Only unmap if this frame still owns the page (it may have
                    // been remapped by a racing load/evict).
                    if shard.get(&old_page) == Some(&(idx as FrameId)) {
                        shard.remove(&old_page);
                    }
                    drop(shard);

                    let referenced = frame.desc.clock_ref.swap(false, Ordering::Relaxed);
                    if !referenced {
                        // Cold page: add to ghost queue for scan resistance.
                        self.push_ghost(old_page);
                    }
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
                frame.desc.reset();
                return Ok(Some(idx as FrameId));
            }

            // Dirty frame: if referenced, clear the bit and keep it; otherwise
            // flush it inline so we can reclaim it on a later sweep.
            if state == FrameState::Dirty as u8 {
                if frame.desc.clock_ref.load(Ordering::Relaxed) {
                    frame.desc.clock_ref.store(false, Ordering::Relaxed);
                    continue;
                }
                self.flush_single_frame(fs, idx as FrameId)?;
                continue;
            }

            if state == FrameState::Loading as u8 || state == FrameState::Flushing as u8 {
                // I/O in flight elsewhere; skip and let the next sweep retry.
                continue;
            }
        }

        Ok(None)
    }

    /// Write a single dirty frame back to disk.
    ///
    /// If a [`DoubleWriteBuffer`] is attached, the page is staged there first
    /// and the DW buffer is cleared after the final write succeeds.  This
    /// ensures that a crash between the DW write and the in-place write can be
    /// detected and repaired by [`DoubleWriteBuffer::recover_torn_pages`] on
    /// the next startup.
    ///
    /// The whole checksum-update / DW-stage / in-place-write sequence runs
    /// under the frame's `io_mutex`, so it cannot race a concurrent load or a
    /// second flusher touching the same bytes.
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

        // Mark inflight so the sweeper and other flushers skip it.
        let was_inflight = frame.desc.io_inflight.swap(true, Ordering::Acquire);
        if was_inflight {
            return Ok(()); // another thread is already flushing it
        }

        // A pinned frame may have a live `PageGuard` writer mutating the bytes;
        // flushing it would read bytes that overlap that writer's `&mut`.  Skip
        // it and let a later flush (or `flush_all`) pick it up once unpinned.
        // The `io_inflight` swap above happens-before this check, and a writer
        // calls `wait_io_quiescent()` (which observes `io_inflight`) before
        // mutating, so the pin/flush exclusion is symmetric and race-free.
        if frame.desc.is_pinned() {
            frame.desc.io_inflight.store(false, Ordering::Release);
            return Ok(());
        }

        // Serialise the buffer access (checksum + writes) against any load.
        let _io_guard = frame.io_mutex.lock();

        // Update the checksum on the frame buffer before any write.
        // SAFETY: io_mutex held and io_inflight set; no other writer can exist.
        let buf_mut = unsafe { frame.buf.get_mut() };
        crate::storage::page::SlottedPage::update_checksum_bytes(buf_mut);

        // Stage through the double-write buffer if one is attached, providing
        // torn-page protection for this single-frame flush.
        if let Some(dw) = &self.doublewrite {
            // SAFETY: still under io_mutex; shared read of the just-updated bytes.
            let bytes = unsafe { frame.buf.get() };
            let pages: Vec<(u64, &[u8])> = vec![(page_id, &bytes[..])];
            dw.write_batch(&pages, fs)?;
        }

        let offset = page_id * PAGE_SIZE as u64;
        let handle = fs.open(&self.data_path, false)?;
        {
            // SAFETY: io_mutex held; shared read of the frame bytes for the write.
            let bytes = unsafe { frame.buf.get() };
            handle.write_at(bytes, offset)?;
        }
        handle.sync_data()?;

        // DW buffer can now be cleared: the in-place write completed durably.
        if let Some(dw) = &self.doublewrite {
            // Non-fatal: if clear fails we simply leave the DW entry; recovery
            // will verify checksums and skip pages that are already intact.
            let _ = dw.clear(fs);
        }

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
        if queue.len() >= self.ghost_capacity
            && let Some(old) = queue.pop_front()
        {
            set.remove(&old);
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

    /// Scan all frames and return ids of dirty ones that are not pinned and
    /// not already being flushed.
    pub fn dirty_candidates(&self) -> Vec<FrameId> {
        let mut out = Vec::new();
        for (idx, frame) in self.frames.iter().enumerate() {
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

    /// Evict any frame currently holding `page_id`.
    ///
    /// The frame's bytes are zeroed and its descriptor reset so the frame can
    /// be reclaimed by the CLOCK-Pro sweeper.  This must be called after
    /// freeing a page so that a subsequent reallocation of the same page id
    /// never serves stale bytes from the cache.
    ///
    /// If the page is not resident the call is a no-op.
    pub fn invalidate(&self, page_id: PageId) {
        if page_id == 0 {
            return;
        }
        let shard_idx = Self::shard_index(page_id);
        let fid = {
            let mut shard = self.shards[shard_idx].lock().unwrap();
            match shard.remove(&page_id) {
                Some(fid) if fid == LOADING_SENTINEL => {
                    // A load is in progress; re-insert and let it finish.  The
                    // caller (post free_page) should not normally hit this, but
                    // we must not strand the sentinel.
                    shard.insert(page_id, LOADING_SENTINEL);
                    return;
                }
                Some(fid) => fid,
                None => return, // page not resident
            }
        };

        let frame = self.frame(fid);
        // Wait until any in-flight I/O on this frame completes.  A simple spin
        // is sufficient: the flusher holds the flag for only a few microseconds
        // and we only arrive here after an explicit free_page().
        while frame.desc.io_inflight.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        // Zeroise the buffer so a subsequent fix_page never serves stale data.
        // Take io_mutex to serialise against any flush that may still be racing.
        {
            let _io_guard = frame.io_mutex.lock();
            // SAFETY: io_mutex held and the page is no longer mapped, so no
            // other thread can form a reference to these bytes.
            let buf = unsafe { frame.buf.get_mut() };
            buf.iter_mut().for_each(|b| *b = 0);
        }

        // Reset the descriptor: the frame is now empty.
        frame.desc.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use std::io::Write;

    fn temp_pool(frames: u32) -> (tempfile::TempDir, PosixFileSystem, BufferPool) {
        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        // Pre-allocate enough space for test page ids (up to ~64 pages).
        let file_pages = (frames as u64).max(64);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.set_len(file_pages * PAGE_SIZE as u64).unwrap();
            f.flush().unwrap();
        }
        // Initialize all pages with valid checksums so fix_page does not fail.
        let handle = fs.open(&path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);
        let pool = BufferPool::new(frames, path);
        (dir, fs, pool)
    }

    #[test]
    fn fix_page_zero_rejected() {
        let (_dir, fs, pool) = temp_pool(4);
        let err = pool.fix_page(&fs, 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn fix_unreferenced_page_reads_from_disk() {
        use crate::storage::page::{PageType, SlottedPage};
        let (_dir, fs, pool) = temp_pool(4);
        // Write a valid page with known data in the data area at page 3.
        let mut page = SlottedPage::init(3, PageType::SlottedData);
        page.buf[SlottedPage::HEADER_SIZE] = 0xCA;
        page.buf[SlottedPage::HEADER_SIZE + 1] = 0xFE;
        page.update_checksum();
        let handle = fs.open(&pool.data_path, false).unwrap();
        handle.write_at(&page.buf, 3 * PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();

        let guard = pool.fix_page(&fs, 3).unwrap();
        assert_eq!(guard.buf()[SlottedPage::HEADER_SIZE], 0xCA);
        assert_eq!(guard.buf()[SlottedPage::HEADER_SIZE + 1], 0xFE);
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
        let frame_id = {
            let guard = pool.fix_page(&fs, 2).unwrap();
            assert_eq!(guard.desc().pin_count.load(Ordering::Relaxed), 1);
            guard.frame_id
        };
        // After drop the pin count is 0 on the frame that held page 2.
        assert_eq!(
            pool.frame(frame_id).desc.pin_count.load(Ordering::Relaxed),
            0
        );
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
        for frame in pool.iter_frames() {
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

    #[test]
    fn as_slice_points_into_frame_without_copy() {
        let (_dir, fs, pool) = temp_pool(4);
        let mut guard = pool.fix_page(&fs, 3).unwrap();
        guard.as_slice_mut()[0] = 0xBE;
        guard.as_slice_mut()[1] = 0xEF;

        let slice = guard.as_slice();
        assert_eq!(slice[0], 0xBE);
        assert_eq!(slice[1], 0xEF);

        // Verify the slice pointer sits inside the frame buffer.
        let frame = pool.frame(guard.frame_id);
        let frame_ptr = frame.buf.as_ptr();
        let slice_ptr = slice.as_ptr();
        assert!(
            slice_ptr >= frame_ptr && slice_ptr < unsafe { frame_ptr.add(frame.buf.len()) },
            "slice must point into the pinned frame buffer"
        );
    }

    #[test]
    fn page_handle_alias_works() {
        let (_dir, fs, pool) = temp_pool(4);
        let mut handle: PageHandle<'_> = pool.fix_page(&fs, 4).unwrap();
        handle.as_slice_mut()[0] = 0x01;
        assert_eq!(handle.as_slice()[0], 0x01);
        assert_eq!(handle.desc().pin_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn new_numa_creates_pool_without_panic() {
        let (_dir, fs, pool) = temp_pool(4);
        // Creating a NUMA-aware pool should not panic on any topology.
        let topo = NumaTopology::detect();
        let pool2 = BufferPool::new_numa(4, pool.data_path.clone(), &topo);
        // Verify basic operations still work.
        let guard = pool2.fix_page(&fs, 1).unwrap();
        assert_eq!(guard.desc().pin_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn readahead_prefetches_sequential_pages() {
        use crate::storage::page::{PageType, SlottedPage};
        let (_dir, fs, pool) = temp_pool(64);
        // Pre-write valid pages 10, 11, 12, 13 to disk.
        let handle = fs.open(&pool.data_path, false).unwrap();
        for pid in 10..=13 {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.buf[SlottedPage::HEADER_SIZE] = pid as u8;
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        // Access pages 10, 11, 12 sequentially to trigger readahead.
        let _g10 = pool.fix_page(&fs, 10).unwrap();
        let _g11 = pool.fix_page(&fs, 11).unwrap();
        let _g12 = pool.fix_page(&fs, 12).unwrap();

        // After the third access, readahead should have prefetched page 13.
        let shard_idx = BufferPool::shard_index(13);
        let shard = pool.shards[shard_idx].lock().unwrap();
        assert!(shard.contains_key(&13), "page 13 should have been prefetched");
    }

    #[test]
    fn random_access_does_not_prefetch() {
        use crate::storage::page::{PageType, SlottedPage};
        let (_dir, fs, pool) = temp_pool(64);
        // Pre-write valid pages 100 and 200.
        let handle = fs.open(&pool.data_path, false).unwrap();
        for pid in [100u64, 200] {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.buf[SlottedPage::HEADER_SIZE] = pid as u8;
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        // Access random pages — should not trigger prefetch.
        let _g100 = pool.fix_page(&fs, 100).unwrap();
        let _g200 = pool.fix_page(&fs, 200).unwrap();

        let shard_idx = BufferPool::shard_index(101);
        let shard = pool.shards[shard_idx].lock().unwrap();
        assert!(
            !shard.contains_key(&101),
            "page 101 should NOT be prefetched after random access"
        );
    }

    #[test]
    fn numa_pool_partitions_frames() {
        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        let file_pages = 64u64;
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.set_len(file_pages * PAGE_SIZE as u64).unwrap();
            f.flush().unwrap();
        }
        // Initialize all pages with valid checksums.
        let handle = fs.open(&path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        let topo = NumaTopology {
            node_count: 2,
            cpus_per_node: vec![vec![0], vec![1]],
        };
        let pool = BufferPool::new_numa(4, path, &topo);
        // On Linux with NUMA support, frames are allocated round-robin.
        // On non-Linux or when mbind fails, it falls back to standard alloc.
        // The test simply ensures no panic and that the pool has 4 frames.
        assert_eq!(pool.frame_count, 4);
    }

    #[test]
    fn invalidate_evicts_resident_frame() {
        use crate::storage::page::SlottedPage;
        let (_dir, fs, pool) = temp_pool(4);

        // Fix page 4, write known data, mark dirty, unpin.
        {
            let mut guard = pool.fix_page(&fs, 4).unwrap();
            guard.as_slice_mut()[SlottedPage::HEADER_SIZE] = 0xAB;
            guard.set_dirty(99);
        }

        // Page should be resident.
        let shard_idx = BufferPool::shard_index(4);
        assert!(pool.shards[shard_idx].lock().unwrap().contains_key(&4));

        // Invalidate: frame must be evicted.
        pool.invalidate(4);
        assert!(!pool.shards[shard_idx].lock().unwrap().contains_key(&4));
    }

    #[test]
    fn freed_then_reallocated_page_is_clean() {
        use crate::storage::manager::PageManager;
        use crate::storage::page::{PageType, SlottedPage};
        use std::sync::Arc;

        let (_dir, fs, path) = {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rgraph.db");
            let fs = PosixFileSystem::new(false);
            {
                let mut f = std::fs::File::create(&path).unwrap();
                f.set_len(64 * PAGE_SIZE as u64).unwrap();
                f.flush().unwrap();
            }
            let handle = fs.open(&path, false).unwrap();
            for pid in 0..64u64 {
                let mut page = SlottedPage::init(pid, PageType::SlottedData);
                page.update_checksum();
                handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
            }
            handle.sync_data().unwrap();
            drop(handle);
            (dir, fs, path)
        };

        // Build a pool and attach it to a page manager.
        let pool = Arc::new(BufferPool::new(16, path.clone()));
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.set_pool(&pool);
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        // Allocate a page and write known sentinel data via the pool.
        let pid = pm.allocate_page();
        let mut buf = SlottedPage::init(pid, PageType::SlottedData);
        buf.buf[SlottedPage::HEADER_SIZE] = 0xDE;
        buf.buf[SlottedPage::HEADER_SIZE + 1] = 0xAD;
        buf.update_checksum();
        pm.write_page(&fs, pid, &mut buf.buf).unwrap();

        // Verify the sentinel is in the pool frame.
        {
            let guard = pool.fix_page(&fs, pid).unwrap();
            assert_eq!(guard.as_slice()[SlottedPage::HEADER_SIZE], 0xDE);
        }

        // Free the page: pool frame must be invalidated.
        pm.free_page(pid);
        let shard_idx = BufferPool::shard_index(pid);
        assert!(
            !pool.shards[shard_idx].lock().unwrap().contains_key(&pid),
            "pool must evict frame after free_page"
        );

        // Reallocate the same page id via the free list.
        let pid2 = pm.allocate_page();
        assert_eq!(pid, pid2, "same page id should be reused from free list");

        // Write a clean page to disk so the pool can verify its checksum on load.
        let mut clean = SlottedPage::init(pid2, PageType::SlottedData);
        clean.update_checksum();
        let handle = fs.open(&path, false).unwrap();
        handle.write_at(&clean.buf, pid2 * PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();
        drop(handle);

        // Fix the reallocated page through the pool: must NOT see 0xDE/0xAD.
        let guard = pool.fix_page(&fs, pid2).unwrap();
        assert_ne!(
            guard.as_slice()[SlottedPage::HEADER_SIZE],
            0xDE,
            "reallocated page must not serve stale data from evicted frame"
        );
    }

    #[test]
    fn fix_page_detects_corruption() {
        use crate::storage::page::{PageType, SlottedPage};
        let (_dir, fs, pool) = temp_pool(4);
        // Pre-write a valid page at page 5.
        let handle = fs.open(&pool.data_path, false).unwrap();
        let mut page = SlottedPage::init(5, PageType::SlottedData);
        page.buf[SlottedPage::HEADER_SIZE] = 0xAB;
        page.update_checksum();
        handle.write_at(&page.buf, 5 * PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();
        drop(handle);

        // Corrupt it on disk.
        let handle = fs.open(&pool.data_path, false).unwrap();
        let corrupt_offset = 5 * PAGE_SIZE as u64 + SlottedPage::HEADER_SIZE as u64 + 10;
        handle.write_at(&[0xFF], corrupt_offset).unwrap();
        handle.sync_data().unwrap();
        drop(handle);

        let result = pool.fix_page(&fs, 5);
        assert!(
            result.is_err(),
            "corrupted page should fail checksum verification"
        );
    }

    // -----------------------------------------------------------------
    // Concurrency tests (Task 173)
    // -----------------------------------------------------------------

    /// Build a pool over a freshly initialised data file with `file_pages`
    /// valid pages.  Returns an `Arc<BufferPool>` and an `Arc<PosixFileSystem>`
    /// so worker threads can share both.
    fn concurrent_pool(
        frames: u32,
        file_pages: u64,
    ) -> (tempfile::TempDir, Arc<PosixFileSystem>, Arc<BufferPool>) {
        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = Arc::new(PosixFileSystem::new(false));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.set_len(file_pages * PAGE_SIZE as u64).unwrap();
            f.flush().unwrap();
        }
        let handle = fs.open(&path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);
        let pool = Arc::new(BufferPool::new(frames, path));
        (dir, fs, pool)
    }

    /// Many threads fix the same page concurrently across many iterations.
    /// No data race may occur and every fix must observe the correct bytes.
    #[test]
    fn concurrent_fix_same_page_no_race() {
        const ITERS: usize = 200;
        const THREADS: usize = 8;
        let (_dir, fs, pool) = concurrent_pool(16, 64);

        for _ in 0..ITERS {
            let mut handles = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let pool = pool.clone();
                let fs = fs.clone();
                handles.push(std::thread::spawn(move || {
                    let g = pool.fix_page(fs.as_ref(), 5).expect("fix page 5");
                    // Read the buffer to exercise the shared-borrow path.
                    let _first = g.as_slice()[0];
                    g.frame_id
                }));
            }
            // Every thread that fixed page 5 must share the same frame id.
            let mut ids: Vec<FrameId> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            ids.dedup();
            assert_eq!(ids.len(), 1, "all concurrent fixes must share one frame");
            // Drop all pins for the next iteration by re-fixing nothing; pins
            // were already released when each guard dropped at thread end.
            // Evict page 5 so the next iteration exercises the miss path again.
            pool.invalidate(5);
        }
    }

    /// A pinned frame must never be chosen for eviction even under pressure.
    #[test]
    fn pin_prevents_concurrent_eviction() {
        use std::sync::Barrier;
        // Two frames only, to force eviction pressure.
        let (_dir, fs, pool) = concurrent_pool(2, 64);

        for _ in 0..100 {
            let barrier = Arc::new(Barrier::new(2));
            let pinned_frame = {
                // Thread A pins page 1 and keeps it pinned across B's fix.
                let pool_a = pool.clone();
                let fs_a = fs.clone();
                let barrier_a = barrier.clone();
                let a = std::thread::spawn(move || {
                    let g = pool_a.fix_page(fs_a.as_ref(), 1).unwrap();
                    let fid = g.frame_id;
                    barrier_a.wait();
                    // Hold the pin while B forces eviction pressure.
                    std::thread::yield_now();
                    let still = g.as_slice()[0]; // touch the buffer
                    drop(g);
                    (fid, still)
                });

                let pool_b = pool.clone();
                let fs_b = fs.clone();
                let barrier_b = barrier.clone();
                let b = std::thread::spawn(move || {
                    barrier_b.wait();
                    // page 1 is pinned; the pool must evict a different frame.
                    let g = pool_b.fix_page(fs_b.as_ref(), 2).expect("fix page 2");
                    g.frame_id
                });

                let (fid_a, _) = a.join().unwrap();
                let fid_b = b.join().unwrap();
                assert_ne!(
                    fid_a, fid_b,
                    "pinned frame must not be evicted to serve another page"
                );
                fid_a
            };
            // Clean up for the next iteration.
            pool.invalidate(1);
            pool.invalidate(2);
            let _ = pinned_frame;
        }
    }

    /// The LOADING_SENTINEL must prevent duplicate I/O: two concurrent fixes of
    /// the same page id always converge on a single frame.
    #[test]
    fn loading_sentinel_prevents_duplicate_io() {
        let (_dir, fs, pool) = concurrent_pool(16, 64);

        for _ in 0..200 {
            let pool_a = pool.clone();
            let fs_a = fs.clone();
            let a = std::thread::spawn(move || {
                pool_a.fix_page(fs_a.as_ref(), 7).unwrap().frame_id
            });
            let pool_b = pool.clone();
            let fs_b = fs.clone();
            let b = std::thread::spawn(move || {
                pool_b.fix_page(fs_b.as_ref(), 7).unwrap().frame_id
            });
            let fid_a = a.join().unwrap();
            let fid_b = b.join().unwrap();
            assert_eq!(
                fid_a, fid_b,
                "two concurrent fixes of the same page must share a frame"
            );
            pool.invalidate(7);
        }
    }
}
