use crate::storage::page::{PageId, PAGE_SIZE};
use std::cell::RefCell;

/// State kept per thread to detect sequential access patterns.
#[derive(Debug, Clone, Copy)]
struct ThreadReadaheadState {
    last_page: PageId,
    stride: i64,
    consecutive: u32,
}

impl ThreadReadaheadState {
    fn new() -> Self {
        Self {
            last_page: 0,
            stride: 0,
            consecutive: 0,
        }
    }

    /// Record an access to `page_id`. Returns the start page for prefetch if
    /// a sequential stride has been confirmed.
    ///
    /// A prefetch is triggered after **two consecutive accesses** with the
    /// same positive stride (e.g. 10 → 11 → 12 triggers prefetch at 13).
    fn record(&mut self, page_id: PageId) -> Option<PageId> {
        let stride = page_id as i64 - self.last_page as i64;
        let prefetch_start = if stride == self.stride && stride > 0 && self.consecutive >= 1 {
            // Confirmed sequential pattern — prefetch the next block.
            Some(page_id + stride as u64)
        } else {
            None
        };
        self.last_page = page_id;
        if stride > 0 && stride == self.stride {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            self.stride = stride;
            self.consecutive = 1;
        }
        prefetch_start
    }
}

thread_local! {
    static REAHEAD_STATE: RefCell<ThreadReadaheadState> = RefCell::new(ThreadReadaheadState::new());
}

/// Maximum number of pages to prefetch in one batch.
/// 256 KiB / 8 KiB page = 32 pages.
pub const MAX_PREFETCH_PAGES: usize = 32;

/// Minimum confirmed sequential accesses before prefetch triggers.
pub const SEQUENTIAL_THRESHOLD: u32 = 2;

/// Called on every page access to update the per-thread tracker.
/// Returns the starting page id that should be prefetched, or `None`.
pub fn track_access(page_id: PageId) -> Option<PageId> {
    REAHEAD_STATE.with(|state| {
        let mut s = state.borrow_mut();
        s.record(page_id)
    })
}

/// Reset the per-thread tracker (useful after random jumps).
pub fn reset_tracker() {
    REAHEAD_STATE.with(|state| {
        *state.borrow_mut() = ThreadReadaheadState::new();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sequential_stride() {
        reset_tracker();
        // First access (from 0 → 10): no prefetch, sets seed stride.
        assert_eq!(track_access(10), None);
        // Second access (10 → 11): defines the real stride (+1), no prefetch yet.
        assert_eq!(track_access(11), None);
        // Third access (11 → 12): confirms the +1 stride — prefetch page 13.
        assert_eq!(track_access(12), Some(13));
    }

    #[test]
    fn resets_on_random_jump() {
        reset_tracker();
        track_access(5);
        track_access(6);
        track_access(7); // confirms stride +1
        // Random jump
        assert_eq!(track_access(100), None);
    }
}
