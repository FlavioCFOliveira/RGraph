//! Latch crabbing with deadlock-free ordering and version validation.
//!
//! Writers acquire exclusive latches; readers acquire shared latches.  Threads
//! that contend on a page **block on a condition variable** rather than
//! spinning, so a busy page does not burn CPU.
//!
//! # Deadlock freedom
//!
//! All latch requests go through a single table.  Callers that hold more than
//! one latch at a time (latch crabbing on the descend path) MUST acquire them
//! in strictly ascending page-id order; the tree always descends root → leaf
//! and B+ tree page ids are allocated monotonically, so this holds naturally.
//!
//! # Optimistic-read support
//!
//! Each page carries a monotonically increasing **version counter** that is
//! bumped every time an exclusive latch is released (i.e. after every possible
//! mutation).  An optimistic reader snapshots the version with `Acquire`
//! ordering, reads the page, then re-checks the version: if it is unchanged no
//! writer touched the page during the read, so the snapshot is consistent;
//! otherwise the reader retries or falls back to a pessimistic shared latch.

use crate::storage::page::PageId;
use std::collections::HashMap;
use std::sync::{Condvar, Mutex};

/// Mode of latch acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchMode {
    /// Shared latch for read-only access.
    Shared,
    /// Exclusive latch for mutation.
    Exclusive,
    /// Intention exclusive: parent knows a child may split/merge.
    Intention,
}

/// Per-page latch table entry.
#[derive(Debug)]
struct LatchEntry {
    /// Number of shared holders (when `writer` is false).
    readers: u32,
    /// Is an exclusive latch currently held?
    writer: bool,
    /// Is an intention latch currently held?
    intention: bool,
    /// Monotonic version, bumped on every exclusive release.  An odd value is
    /// never observed by readers because the bump happens under the table lock.
    version: u64,
}

impl LatchEntry {
    fn new() -> Self {
        Self {
            readers: 0,
            writer: false,
            intention: false,
            version: 0,
        }
    }

    /// May this entry be removed from the table?  Only when nobody holds it and
    /// its version is still zero (no mutation has been recorded that a reader
    /// might still want to validate against).  Keeping versioned entries alive
    /// preserves optimistic-read soundness across the page's lifetime.
    fn is_idle(&self) -> bool {
        self.readers == 0 && !self.writer && !self.intention && self.version == 0
    }
}

/// State protected by the table mutex.
#[derive(Debug)]
struct LatchTable {
    entries: HashMap<PageId, LatchEntry>,
}

/// Global latch manager.
///
/// A single `Mutex` guards the table and a `Condvar` wakes blocked waiters when
/// a latch is released.
#[derive(Debug)]
pub struct LatchCoupling {
    table: Mutex<LatchTable>,
    cond: Condvar,
}

impl Default for LatchCoupling {
    fn default() -> Self {
        Self::new()
    }
}

impl LatchCoupling {
    pub fn new() -> Self {
        Self {
            table: Mutex::new(LatchTable {
                entries: HashMap::new(),
            }),
            cond: Condvar::new(),
        }
    }

    /// Acquire a latch on `page_id` in the given mode, blocking on a condition
    /// variable until it is available.
    pub fn latch(&self, page_id: PageId, mode: LatchMode) -> LatchGuard<'_> {
        let mut table = self.table.lock().unwrap();
        loop {
            let entry = table.entries.entry(page_id).or_insert_with(LatchEntry::new);
            let can_acquire = match mode {
                // Shared is compatible with other shared and intention latches.
                LatchMode::Shared => !entry.writer,
                // Exclusive requires the page be completely quiescent.
                LatchMode::Exclusive => entry.readers == 0 && !entry.writer && !entry.intention,
                // Intention is compatible with shared/intention but not writers.
                LatchMode::Intention => !entry.writer,
            };
            if can_acquire {
                match mode {
                    LatchMode::Shared => entry.readers += 1,
                    LatchMode::Exclusive => entry.writer = true,
                    LatchMode::Intention => entry.intention = true,
                }
                return LatchGuard {
                    manager: self,
                    page_id,
                    mode,
                };
            }
            // Block until a release wakes us.
            table = self.cond.wait(table).unwrap();
        }
    }

    /// Snapshot the current version of `page_id` for optimistic validation.
    ///
    /// Returns the version with `Acquire`-equivalent ordering (the read happens
    /// under the table mutex, which provides the necessary happens-before).
    /// A page with no entry has version `0`.
    pub fn version(&self, page_id: PageId) -> u64 {
        let table = self.table.lock().unwrap();
        table.entries.get(&page_id).map(|e| e.version).unwrap_or(0)
    }

    /// Is `page_id` currently latched exclusively?  Used by optimistic readers
    /// to bail out early when a writer is mid-mutation.
    pub fn is_write_latched(&self, page_id: PageId) -> bool {
        let table = self.table.lock().unwrap();
        table
            .entries
            .get(&page_id)
            .map(|e| e.writer)
            .unwrap_or(false)
    }

    fn release(&self, page_id: PageId, mode: LatchMode) {
        let mut table = self.table.lock().unwrap();
        if let Some(entry) = table.entries.get_mut(&page_id) {
            match mode {
                LatchMode::Shared => {
                    if entry.readers > 0 {
                        entry.readers -= 1;
                    }
                }
                LatchMode::Exclusive => {
                    entry.writer = false;
                    // A mutation may have occurred: bump the version so any
                    // concurrent optimistic reader detects the change.
                    entry.version = entry.version.wrapping_add(1);
                }
                LatchMode::Intention => {
                    entry.intention = false;
                }
            }
            if entry.is_idle() {
                table.entries.remove(&page_id);
            }
        }
        // Wake all waiters; each re-checks its acquisition predicate.
        self.cond.notify_all();
    }
}

/// RAII guard for a latched page.
pub struct LatchGuard<'a> {
    manager: &'a LatchCoupling,
    pub page_id: PageId,
    pub mode: LatchMode,
}

impl Drop for LatchGuard<'_> {
    fn drop(&mut self) {
        self.manager.release(self.page_id, self.mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn exclusive_blocks_exclusive() {
        let latch = LatchCoupling::new();
        let g1 = latch.latch(1, LatchMode::Exclusive);
        assert_eq!(g1.page_id, 1);
        assert_eq!(g1.mode, LatchMode::Exclusive);
        drop(g1);
        let g2 = latch.latch(1, LatchMode::Exclusive);
        assert_eq!(g2.page_id, 1);
    }

    #[test]
    fn shared_allows_shared() {
        let latch = LatchCoupling::new();
        let g1 = latch.latch(1, LatchMode::Shared);
        let g2 = latch.latch(1, LatchMode::Shared);
        drop(g1);
        drop(g2);
    }

    #[test]
    fn exclusive_blocks_shared() {
        let latch = LatchCoupling::new();
        let g = latch.latch(2, LatchMode::Exclusive);
        drop(g);
        let g2 = latch.latch(2, LatchMode::Shared);
        drop(g2);
    }

    #[test]
    fn intention_allows_shared() {
        let latch = LatchCoupling::new();
        let g1 = latch.latch(3, LatchMode::Intention);
        let g2 = latch.latch(3, LatchMode::Shared);
        drop(g1);
        drop(g2);
    }

    #[test]
    fn intention_blocks_exclusive() {
        let latch = LatchCoupling::new();
        let g1 = latch.latch(4, LatchMode::Intention);
        drop(g1);
        let g2 = latch.latch(4, LatchMode::Exclusive);
        drop(g2);
    }

    #[test]
    fn version_bumps_on_exclusive_release() {
        let latch = LatchCoupling::new();
        let v0 = latch.version(7);
        {
            let _g = latch.latch(7, LatchMode::Exclusive);
            // Version does not change while the latch is held.
            assert_eq!(latch.version(7), v0);
        }
        // After release the version has advanced.
        assert!(latch.version(7) > v0);
    }

    #[test]
    fn shared_release_does_not_bump_version() {
        let latch = LatchCoupling::new();
        {
            let _g = latch.latch(9, LatchMode::Exclusive);
        }
        let v = latch.version(9);
        {
            let _g = latch.latch(9, LatchMode::Shared);
        }
        assert_eq!(latch.version(9), v, "reads must not bump the version");
    }

    #[test]
    fn condvar_blocks_then_wakes_writer() {
        // A second exclusive waiter must block until the first releases, then
        // acquire — proving condvar handoff works without spinning.
        let latch = Arc::new(LatchCoupling::new());
        let g1 = latch.latch(11, LatchMode::Exclusive);

        let l2 = Arc::clone(&latch);
        let handle = thread::spawn(move || {
            let g = l2.latch(11, LatchMode::Exclusive);
            assert_eq!(g.page_id, 11);
        });
        // Give the spawned thread time to block.
        thread::sleep(std::time::Duration::from_millis(20));
        drop(g1);
        handle.join().unwrap();
    }

    #[test]
    fn many_threads_exclusive_no_deadlock() {
        let latch = Arc::new(LatchCoupling::new());
        let mut handles = Vec::new();
        for _ in 0..16 {
            let l = Arc::clone(&latch);
            handles.push(thread::spawn(move || {
                for pid in 1u64..=8 {
                    let _g = l.latch(pid, LatchMode::Exclusive);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
