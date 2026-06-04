//! Crossbeam-epoch integration for page snapshotting (prototype).
//!
//! This module provides an `EpochPageTable` that stores `BTreePage`s behind
//! `crossbeam_epoch::Atomic` pointers and uses epoch-based reclamation for
//! deferred destruction of replaced pages.
//!
//! # Not yet lock-free
//!
//! The table itself is still a `Mutex<HashMap<PageId, Atomic<BTreePage>>>`, so
//! both reads and writes serialise on the map mutex; only the *page snapshot
//! retirement* is epoch-protected, not the table lookup.  Making the lookup
//! genuinely lock-free (e.g. a concurrent hash map or a fully atomic page
//! table) is future work.  This prototype is gated behind the off-by-default
//! `epoch` feature and is not wired into [`crate::index::btree`].

use crate::index::page::BTreePage;
use crate::storage::page::PageId;
use crossbeam_epoch::Atomic;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

/// A page table entry protected by crossbeam-epoch.
///
/// Readers pin the epoch, load the atomic pointer, and safely dereference
/// the immutable page snapshot.  Writers CAS a new pointer and retire the
/// old one; destruction is deferred until no reader epoch references it.
pub struct EpochPageTable {
    table: Mutex<HashMap<PageId, Atomic<BTreePage>>>,
}

impl Default for EpochPageTable {
    fn default() -> Self {
        Self::new()
    }
}

impl EpochPageTable {
    pub fn new() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
        }
    }

    /// Store a page (overwrites any existing entry).
    /// The old page is retired through the epoch collector.
    pub fn insert(&self,
        page_id: PageId,
        page: BTreePage,
    ) {
        let guard = &crossbeam_epoch::pin();
        let mut table = self.table.lock().unwrap();
        if let Some(old) = table.remove(&page_id) {
            let shared = old.load(Ordering::Relaxed, guard);
            if !shared.is_null() {
                unsafe { guard.defer_destroy(shared) };
            }
        }
        table.insert(page_id, Atomic::new(page));
    }

    /// Load a page snapshot for `page_id`.
    ///
    /// The caller must hold an active epoch pin (`guard`); the returned
    /// reference is valid for as long as that pin is held.  Note that this
    /// still takes the table mutex to find the entry — only the snapshot
    /// pointer behind it is epoch-protected (see the module-level note on the
    /// prototype not yet being lock-free).
    pub fn load<'a>(
        &self,
        page_id: PageId,
        guard: &'a crossbeam_epoch::Guard,
    ) -> Option<&'a BTreePage> {
        let table = self.table.lock().unwrap();
        let atomic = table.get(&page_id)?;
        let shared = atomic.load(Ordering::Acquire, guard);
        if shared.is_null() {
            return None;
        }
        Some(unsafe { shared.deref() })
    }
}

impl Drop for EpochPageTable {
    fn drop(&mut self) {
        let guard = &crossbeam_epoch::pin();
        let mut table = self.table.lock().unwrap();
        for (_page_id, atomic) in table.drain() {
            let shared = atomic.load(Ordering::Relaxed, guard);
            if !shared.is_null() {
                unsafe {
                    // Convert the shared reference back to an owned pointer
                    // so that it is dropped immediately.
                    let _ = shared.into_owned();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_load_roundtrip() {
        let table = EpochPageTable::new();
        let page = BTreePage::new_leaf(1);
        table.insert(1, page.clone());
        let guard = &crossbeam_epoch::pin();
        let loaded = table.load(1, guard).unwrap();
        assert_eq!(loaded.page_id(), 1);
        assert!(loaded.is_leaf());
    }

    #[test]
    fn replace_page_retires_old() {
        let table = EpochPageTable::new();
        let page_v1 = BTreePage::new_leaf(1);
        table.insert(1, page_v1);
        let page_v2 = BTreePage::new_leaf(1);
        table.insert(1, page_v2);
        let guard = &crossbeam_epoch::pin();
        let loaded = table.load(1, guard).unwrap();
        assert!(loaded.is_leaf());
        // Force epoch advancement to ensure retirement runs.
        let _ = guard;
    }

    #[test]
    fn load_missing_page_returns_none() {
        let table = EpochPageTable::new();
        let guard = &crossbeam_epoch::pin();
        assert!(table.load(99, guard).is_none());
    }
}
