use crate::io::AlignedBuffer;
use crate::storage::page::PAGE_SIZE;
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

/// A frame owns both its descriptor and its aligned page buffer.
#[derive(Debug)]
pub struct Frame {
    pub desc: FrameDescriptor,
    pub buf: AlignedBuffer,
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
            buf: AlignedBuffer::zeroed(PAGE_SIZE),
        }
    }

    /// Create a frame backed by an existing buffer.
    pub fn with_buffer(buf: AlignedBuffer) -> Self {
        assert_eq!(buf.len(), PAGE_SIZE, "frame buffer must be PAGE_SIZE");
        Self {
            desc: FrameDescriptor::new(),
            buf,
        }
    }
}

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
