use crate::io::AlignedBuffer;
use crate::storage::page::PAGE_SIZE;
use parking_lot::Mutex as ParkingMutex;
use std::cell::UnsafeCell;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};

/// Dense index into the frame table.
pub type FrameId = u32;

/// Sentinel value meaning "not in the frame table".
pub const INVALID_FRAME_ID: FrameId = u32::MAX;

/// Lifecycle state of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameState {
    /// Frame is empty and can be used for a new page.
    Empty = 0,
    /// Frame contains a clean page that is identical to disk.
    Clean = 1,
    /// Frame contains a page that has been modified in memory.
    Dirty = 2,
    /// Frame is being written back to disk by the flusher.
    Flushing = 3,
    /// Frame is being read from disk into memory.
    Loading = 4,
}

/// Fixed-size descriptor for a single buffer-pool frame.
///
/// All fields are atomics so the flusher, clock sweeper, and hot-path
/// pin/unpin can operate concurrently without a global mutex.
#[derive(Debug)]
pub struct FrameDescriptor {
    /// The page currently resident in this frame, or `0` if empty.
    pub page_id: AtomicU64,
    /// Number of threads (or requests) currently accessing this frame.
    pub pin_count: AtomicU16,
    /// Has the frame been modified since it was read from disk?
    pub dirty: AtomicBool,
    /// CLOCK-Pro referenced bit (set on every access, cleared by sweeper).
    pub clock_ref: AtomicBool,
    /// LSN of the most recent modification to this frame.
    pub last_lsn: AtomicU64,
    /// LSN of the earliest unflushed modification (recovery needs this).
    pub rec_lsn: AtomicU64,
    /// Is there an outstanding I/O operation on this frame?
    pub io_inflight: AtomicBool,
    /// Current lifecycle state.
    pub state: AtomicU8,
}

impl Default for FrameDescriptor {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDescriptor {
    pub fn new() -> Self {
        Self {
            page_id: AtomicU64::new(0),
            pin_count: AtomicU16::new(0),
            dirty: AtomicBool::new(false),
            clock_ref: AtomicBool::new(false),
            last_lsn: AtomicU64::new(0),
            rec_lsn: AtomicU64::new(u64::MAX),
            io_inflight: AtomicBool::new(false),
            state: AtomicU8::new(FrameState::Empty as u8),
        }
    }

    /// Quick check: is this frame currently pinned?
    pub fn is_pinned(&self) -> bool {
        self.pin_count.load(Ordering::Relaxed) > 0
    }

    /// Quick check: is this frame eligible for eviction?
    /// Eligible means empty, not pinned, and no I/O in flight.
    pub fn is_evictable(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);
        if state == FrameState::Loading as u8 || state == FrameState::Flushing as u8 {
            return false;
        }
        self.pin_count.load(Ordering::Relaxed) == 0 && !self.io_inflight.load(Ordering::Relaxed)
    }

    /// Reset descriptor to empty state (caller must own the frame).
    pub fn reset(&self) {
        self.page_id.store(0, Ordering::Relaxed);
        self.pin_count.store(0, Ordering::Relaxed);
        self.dirty.store(false, Ordering::Relaxed);
        self.clock_ref.store(false, Ordering::Relaxed);
        self.last_lsn.store(0, Ordering::Relaxed);
        self.rec_lsn.store(u64::MAX, Ordering::Relaxed);
        self.io_inflight.store(false, Ordering::Relaxed);
        self.state.store(FrameState::Empty as u8, Ordering::Relaxed);
    }
}

/// Interior-mutable wrapper around a frame's page buffer.
///
/// # Why `UnsafeCell`
///
/// The buffer pool hands out `&Frame` references to many callers
/// simultaneously (the background flusher, the CLOCK sweeper, the
/// checkpointer, and `PageGuard` holders).  Yet exactly one of those callers
/// must be able to *mutate* a given frame's bytes at a time.  `UnsafeCell`
/// provides that interior mutability without violating Rust's aliasing model,
/// provided the two access disciplines below are respected.
///
/// # Aliasing discipline (the invariant that makes the `unsafe` sound)
///
/// A mutable reference to the inner [`AlignedBuffer`] may only be formed when
/// the caller holds **one** of:
///
/// 1. The owning frame's `io_mutex` (the I/O path: load-from-disk and
///    write-back-to-disk).  `io_inflight` is also set so the CLOCK sweeper
///    and other flushers skip the frame, and the `LOADING_SENTINEL` keeps the
///    miss path from publishing the frame before its first load completes.
/// 2. A `&mut PageGuard` for the frame, which is unique by construction and
///    whose `pin_count > 0` keeps the frame from being evicted or loaded
///    underneath it.
///
/// Shared (`&`) reads of the buffer require either of the above, or simply a
/// live `PageGuard` (pin held) when no writer can be active.
///
/// [`Deref`] exposes the buffer **read-only** so ergonomic indexing
/// (`frame.buf[0]`, `&frame.buf[..]`, `frame.buf.len()`) keeps working; these
/// shared reads are sound under the same pin / `io_mutex` discipline.
#[derive(Debug)]
pub struct FrameBuf {
    inner: UnsafeCell<AlignedBuffer>,
}

impl FrameBuf {
    /// Wrap an existing aligned buffer.
    #[inline]
    pub fn new(buf: AlignedBuffer) -> Self {
        Self {
            inner: UnsafeCell::new(buf),
        }
    }

    /// Raw shared pointer to the underlying buffer.
    ///
    /// # Safety
    ///
    /// The caller must respect the aliasing discipline documented on
    /// [`FrameBuf`]: no `&mut` to the same buffer may be live concurrently.
    #[inline]
    pub(crate) unsafe fn get(&self) -> &AlignedBuffer {
        // SAFETY: caller upholds the FrameBuf aliasing discipline.
        unsafe { &*self.inner.get() }
    }

    /// Raw exclusive pointer to the underlying buffer.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning frame's `io_mutex` **or** a
    /// `&mut PageGuard` for the frame, guaranteeing no other reference to the
    /// buffer is live.  See the [`FrameBuf`] aliasing discipline.
    #[inline]
    #[allow(clippy::mut_from_ref)] // Intentional: UnsafeCell interior mutability.
    pub(crate) unsafe fn get_mut(&self) -> &mut AlignedBuffer {
        // SAFETY: caller upholds the FrameBuf aliasing discipline (exclusive).
        unsafe { &mut *self.inner.get() }
    }
}

impl Deref for FrameBuf {
    type Target = AlignedBuffer;

    /// Read-only view of the buffer.
    ///
    /// This is sound under the [`FrameBuf`] aliasing discipline: a shared
    /// borrow is only created while a pin is held or under `io_mutex`, where
    /// no concurrent writer exists.
    #[inline]
    fn deref(&self) -> &AlignedBuffer {
        // SAFETY: shared read under the pin / io_mutex discipline.
        unsafe { &*self.inner.get() }
    }
}

/// A frame owns both its descriptor and its aligned page buffer.
///
/// The buffer is wrapped in [`FrameBuf`] so the pool can mutate a single
/// frame's bytes through a shared `&Frame` while keeping Rust's aliasing
/// model intact.  Per-frame I/O is serialised by [`Frame::io_mutex`].
#[derive(Debug)]
pub struct Frame {
    pub desc: FrameDescriptor,
    pub buf: FrameBuf,
    /// Serialises background flush and load-from-disk I/O for this frame's
    /// bytes.  The normal page-access path (held `PageGuard`) does **not**
    /// acquire this lock; pin exclusivity covers it instead.
    pub(crate) io_mutex: ParkingMutex<()>,
}

impl Default for Frame {
    fn default() -> Self {
        Self::new()
    }
}

impl Frame {
    pub fn new() -> Self {
        Self {
            desc: FrameDescriptor::new(),
            buf: FrameBuf::new(AlignedBuffer::zeroed(PAGE_SIZE)),
            io_mutex: ParkingMutex::new(()),
        }
    }

    /// Create a frame backed by an existing buffer.
    pub fn with_buffer(buf: AlignedBuffer) -> Self {
        assert_eq!(buf.len(), PAGE_SIZE, "frame buffer must be PAGE_SIZE");
        Self {
            desc: FrameDescriptor::new(),
            buf: FrameBuf::new(buf),
            io_mutex: ParkingMutex::new(()),
        }
    }

    /// Block until no I/O is in flight on this frame, establishing a
    /// happens-before edge with the I/O path before the caller touches bytes.
    ///
    /// Used by the `PageGuard` write accessors: a pinned writer waits out any
    /// flush that started just before it acquired the pin.  Because every flush
    /// path skips pinned frames, the pin then keeps further flushes away, so
    /// the writer obtains exclusive access to the buffer bytes.
    ///
    /// The `io_mutex` round-trip provides the acquire/release fence; the
    /// `io_inflight` check provides the liveness condition.
    pub(crate) fn wait_io_quiescent(&self) {
        loop {
            {
                let _io_guard = self.io_mutex.lock();
                if !self.desc.io_inflight.load(Ordering::Acquire) {
                    return;
                }
            }
            std::thread::yield_now();
        }
    }
}

// SAFETY: A `Frame` is `Sync` because:
//  * `desc` is composed solely of atomics.
//  * `buf` is an `UnsafeCell<AlignedBuffer>` whose backing memory is plain
//    page-aligned bytes (`AlignedBuffer` is itself `Send + Sync`).  All
//    mutable access is funnelled through the `FrameBuf` aliasing discipline
//    (held `io_mutex` on the I/O path, or `&mut PageGuard` with `pin_count > 0`
//    on the access path), so no two threads ever form overlapping `&mut`
//    references to the same buffer.
//  * `io_mutex` is itself `Sync`.
// `Send` follows for the same reasons: ownership of a frame can move between
// threads safely because the bytes carry no thread affinity.
unsafe impl Sync for Frame {}
unsafe impl Send for Frame {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_is_empty() {
        let f = Frame::new();
        assert_eq!(f.desc.state.load(Ordering::Relaxed), FrameState::Empty as u8);
        assert!(!f.desc.is_pinned());
        assert!(f.desc.is_evictable());
    }

    #[test]
    fn pin_prevents_eviction() {
        let f = Frame::new();
        f.desc.pin_count.store(1, Ordering::Relaxed);
        assert!(!f.desc.is_evictable());
        f.desc.pin_count.store(0, Ordering::Relaxed);
        assert!(f.desc.is_evictable());
    }

    #[test]
    fn io_inflight_prevents_eviction() {
        let f = Frame::new();
        f.desc.io_inflight.store(true, Ordering::Relaxed);
        assert!(!f.desc.is_evictable());
        f.desc.io_inflight.store(false, Ordering::Relaxed);
        assert!(f.desc.is_evictable());
    }

    #[test]
    fn reset_clears_all_state() {
        let f = Frame::new();
        f.desc.page_id.store(42, Ordering::Relaxed);
        f.desc.dirty.store(true, Ordering::Relaxed);
        f.desc.pin_count.store(3, Ordering::Relaxed);
        f.desc.last_lsn.store(100, Ordering::Relaxed);
        f.desc.state.store(FrameState::Dirty as u8, Ordering::Relaxed);

        f.desc.reset();

        assert_eq!(f.desc.page_id.load(Ordering::Relaxed), 0);
        assert!(!f.desc.dirty.load(Ordering::Relaxed));
        assert_eq!(f.desc.pin_count.load(Ordering::Relaxed), 0);
        assert_eq!(f.desc.last_lsn.load(Ordering::Relaxed), 0);
        assert_eq!(f.desc.state.load(Ordering::Relaxed), FrameState::Empty as u8);
    }

    #[test]
    fn frame_buffer_is_page_size() {
        let f = Frame::new();
        assert_eq!(f.buf.len(), PAGE_SIZE);
        assert_eq!(f.buf.as_ptr() as usize % AlignedBuffer::ALIGNMENT, 0);
    }
}
