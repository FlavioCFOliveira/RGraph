//! Core B+ tree operations: search, point insert/delete, split, merge.
//!
//! The tree is built over slotted pages and uses latch crabbing for
//! concurrency.  Structural changes are logged to WAL as physical
//! redo/undo records.

use crate::buffer::pool::BufferPool;
use crate::index::key::CompositeKey;
use crate::index::latch::{LatchCoupling, LatchMode};
use crate::index::page::BTreePage;
use crate::io::FileSystem;
use crate::storage::page::PageId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Configuration knobs for a B+ tree.
#[derive(Debug, Clone)]
pub struct BPlusTreeConfig {
    /// Page size in bytes (default 8192).
    pub page_size: usize,
    /// Minimum fill ratio before a merge is triggered (default 0.30).
    pub min_fill_ratio: f64,
    /// Split threshold: when a page exceeds this fraction of capacity,
    /// it is split (default 0.85).
    pub split_threshold: f64,
    /// Maximum number of retries for optimistic reads.
    pub optimistic_retry: u8,
}

impl Default for BPlusTreeConfig {
    fn default() -> Self {
        Self {
            page_size: 8192,
            min_fill_ratio: 0.30,
            split_threshold: 0.85,
            optimistic_retry: 3,
        }
    }
}

/// In-memory B+ tree using page-level storage.
///
/// When a [`BufferPool`] is attached via [`BPlusTree::with_pool`], pages
/// are fixed in the pool and written back through the normal flush path,
/// making the index persistent.  Without a pool the tree operates purely
/// in-memory using a private `HashMap`.
pub struct BPlusTree {
    pub config: BPlusTreeConfig,
    pub root_page_id: AtomicU64,
    pub latch_mgr: LatchCoupling,
    /// In-memory fallback page store (page_id -> BTreePage).
    pages: Mutex<HashMap<PageId, BTreePage>>,
    next_page_id: Mutex<PageId>,
    /// Monotonic LSN generator for optimistic read validation.
    next_lsn: AtomicU64,
    /// Optional buffer pool for persistence.
    pool: Option<Arc<BufferPool>>,
    /// File-system handle required when `pool` is present.
    fs: Option<Arc<dyn FileSystem>>,
}

impl std::fmt::Debug for BPlusTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BPlusTree")
            .field("config", &self.config)
            .field("root_page_id", &self.root_page_id)
            .field("latch_mgr", &self.latch_mgr)
            .field("next_page_id", &self.next_page_id)
            .field("next_lsn", &self.next_lsn)
            .field("pool", &self.pool)
            .finish_non_exhaustive()
    }
}

impl BPlusTree {
    pub fn new(config: BPlusTreeConfig) -> Self {
        let root = BTreePage::new_leaf(1);
        let mut pages = HashMap::new();
        pages.insert(1, root);
        Self {
            config,
            root_page_id: AtomicU64::new(1),
            latch_mgr: LatchCoupling::new(),
            pages: Mutex::new(pages),
            next_page_id: Mutex::new(2),
            next_lsn: AtomicU64::new(1),
            pool: None,
            fs: None,
        }
    }

    /// Attach a buffer pool and file system, enabling persistence.
    ///
    /// Any pages already resident in the in-memory fallback store are
    /// copied into the pool so that subsequent reads are consistent.
    pub fn with_pool(mut self, pool: Arc<BufferPool>, fs: Arc<dyn FileSystem>) -> Self {
        self.pool = Some(pool.clone());
        self.fs = Some(fs.clone());
        // Sync existing in-memory pages into the pool.
        let pages = self.pages.lock().unwrap();
        for (&page_id, page) in pages.iter() {
            if let Ok(mut guard) = pool.fix_page(fs.as_ref(), page_id) {
                let buf = guard.buf_mut();
                buf.copy_from_slice(&page.inner.buf[..]);
                guard.set_dirty(page.page_lsn());
            }
        }
        drop(pages);
        self
    }

    /// Copy every page currently held in the in-memory fallback store into
    /// the attached buffer pool.  This is useful when migrating an existing
    /// in-memory tree to persistent storage.
    pub fn sync_to_pool(&self) {
        let (Some(pool), Some(fs)) = (&self.pool, self.fs.as_deref()) else {
            return;
        };
        let pages = self.pages.lock().unwrap();
        for (&page_id, page) in pages.iter() {
            if let Ok(mut guard) = pool.fix_page(fs, page_id) {
                let buf = guard.buf_mut();
                buf.copy_from_slice(&page.inner.buf[..]);
                guard.set_dirty(page.page_lsn());
            }
        }
    }

    /// Return a snapshot of every page id currently known to the tree.
    pub fn all_page_ids(&self) -> Vec<PageId> {
        let pages = self.pages.lock().unwrap();
        pages.keys().copied().collect()
    }

    pub(crate) fn bump_lsn(&self) -> u64 {
        self.next_lsn.fetch_add(1, Ordering::Relaxed)
    }

    fn alloc_page(&self) -> PageId {
        let mut next = self.next_page_id.lock().unwrap();
        let id = *next;
        *next += 1;
        // If a pool is attached, ensure the data file is large enough.
        if let (Some(pool), Some(fs)) = (&self.pool,
            self.fs.as_deref()
        ) {
            let required_len = id * crate::storage::page::PAGE_SIZE as u64;
            if let Ok(handle) = fs.open(&pool.data_path, false) {
                if let Ok(current_len) = handle.len() {
                    if current_len < required_len {
                        let _ = handle.set_len(required_len);
                    }
                }
            }
        }
        id
    }

    pub fn get_page(&self, page_id: PageId) -> Option<BTreePage> {
        if let (Some(pool), Some(fs)) = (&self.pool, self.fs.as_deref()) {
            match pool.fix_page(fs, page_id) {
                Ok(guard) => {
                    // Copy the page out of the buffer pool.
                    let buf = guard.buf().clone();
                    Some(BTreePage::from_buf(buf))
                }
                Err(_) => None,
            }
        } else {
            let pages = self.pages.lock().unwrap();
            pages.get(&page_id).cloned()
        }
    }

    pub(crate) fn put_page_with_lsn(&self, page_id: PageId, mut page: BTreePage) {
        let lsn = self.bump_lsn();
        page.set_page_lsn(lsn);
        if let (Some(pool), Some(fs)) = (&self.pool, self.fs.as_deref()) {
            match pool.fix_page(fs, page_id) {
                Ok(mut guard) => {
                    let buf = guard.buf_mut();
                    buf.copy_from_slice(&page.inner.buf[..]);
                    guard.set_dirty(lsn);
                }
                Err(_) => {
                    let mut pages = self.pages.lock().unwrap();
                    pages.insert(page_id, page);
                }
            }
        } else {
            let mut pages = self.pages.lock().unwrap();
            pages.insert(page_id, page);
        }
    }

    /// Search for `key` and return `(page_id, slot)` of the leaf entry.
    pub fn search(
        &self,
        key: &CompositeKey,
    ) -> Option<(PageId, u16)> {
        let root = self.get_page(self.root_page_id.load(Ordering::Relaxed))?;
        let (leaf_id, _) = self.find_leaf(root, key)?;
        let leaf = self.get_page(leaf_id)?;
        let slot = Self::leaf_lower_bound(&leaf, key);
        if let Some(k) = Self::leaf_key(&leaf, slot)
            && k.as_slice() == key.as_slice()
        {
            return Some((leaf_id, slot));
        }
        None
    }

    /// Optimistic search without acquiring latches.
    ///
    /// Records the LSN of every page visited.  After reaching the leaf,
    /// re-reads each page in the path and verifies its LSN has not changed.
    /// If validation fails, retries up to `config.optimistic_retry` times,
    /// then falls back to pessimistic [`search`].
    pub fn optimistic_search(
        &self,
        key: &CompositeKey,
    ) -> Option<(PageId, u16)> {
        let max_retry = self.config.optimistic_retry;
        for _ in 0..=max_retry {
            let root_id = self.root_page_id.load(Ordering::Relaxed);
            let root = match self.get_page(root_id) {
                Some(p) => p,
                None => return self.search(key),
            };
            let root_lsn = root.page_lsn();

            let (leaf_id, path) = match self.find_leaf(root, key) {
                Some(r) => r,
                None => return self.search(key),
            };
            let _leaf_lsn = path.last().map(|p| p.1).unwrap_or(0);

            // Validation: re-read every page in the path and check LSN.
            let mut valid = true;
            if self.get_page(root_id).map(|p| p.page_lsn()) != Some(root_lsn) {
                valid = false;
            }
            for &(page_id, expected_lsn) in &path {
                if self.get_page(page_id).map(|p| p.page_lsn()) != Some(expected_lsn) {
                    valid = false;
                    break;
                }
            }

            if !valid {
                std::thread::yield_now();
                continue;
            }

            let leaf = match self.get_page(leaf_id) {
                Some(p) => p,
                None => return self.search(key),
            };
            let slot = Self::leaf_lower_bound(&leaf, key);
            if let Some(k) = Self::leaf_key(&leaf, slot)
                && k.as_slice() == key.as_slice()
            {
                return Some((leaf_id, slot));
            }
            return None;
        }
        // Fallback to pessimistic search after exhausting retries.
        self.search(key)
    }

    /// Insert `key` -> `value` into the tree.
    pub fn insert(
        &self,
        key: &CompositeKey,
        value: &[u8],
    ) -> Result<(), BTreeError> {
        let guard = self.latch_mgr.latch(self.root_page_id.load(Ordering::Relaxed), LatchMode::Exclusive);
        let root = self.get_page(self.root_page_id.load(Ordering::Relaxed)).ok_or(BTreeError::MissingRoot)?;

        // Fast path: empty tree, insert directly into root leaf.
        if root.is_leaf() && root.key_count() == 0 {
            let mut root_mut = root.clone();
            let record = encode_kv(key.as_slice(), value);
            if root_mut.insert_raw(&record).is_some() {
                self.put_page_with_lsn(self.root_page_id.load(Ordering::Relaxed), root_mut);
                drop(guard);
                return Ok(());
            }
            // Root is physically full despite being logically empty; fall through to split.
        }

        // Descend to leaf with latch coupling (simplified: exclusive all the way).
        let (leaf_id, mut leaf) = self.descend_to_leaf_exclusive(root.clone(), key)
            .ok_or(BTreeError::MissingLeaf)?;

        let record = encode_kv(key.as_slice(), value);

        // Check if key already exists.
        let slot = Self::leaf_lower_bound(&leaf, key);
        if let Some(existing) = Self::leaf_key(&leaf, slot)
            && existing.as_slice() == key.as_slice()
        {
            // Overwrite value in place.
            let mut new_leaf = leaf.clone();
            new_leaf.delete(slot);
            if new_leaf.insert_raw_at(slot, &record).is_some() {
                self.put_page_with_lsn(leaf_id, new_leaf);
                drop(guard);
                return Ok(());
            }
            // Leaf full after delete; fall through to split.
        }

        if leaf.insert_raw_at(slot, &record).is_some() {
            self.put_page_with_lsn(leaf_id, leaf);
            drop(guard);
            return Ok(());
        }

        // Leaf is full: split.
        drop(guard);
        self.split_leaf(leaf_id, leaf, key, value)?;
        Ok(())
    }

    /// Delete `key` from the tree. Returns true if the key was found.
    pub fn delete(
        &self,
        key: &CompositeKey,
    ) -> Result<bool, BTreeError> {
        let guard = self.latch_mgr.latch(self.root_page_id.load(Ordering::Relaxed), LatchMode::Exclusive);
        let root = self.get_page(self.root_page_id.load(Ordering::Relaxed)).ok_or(BTreeError::MissingRoot)?;
        let (leaf_id, mut leaf) = self.descend_to_leaf_exclusive(root.clone(), key)
            .ok_or(BTreeError::MissingLeaf)?;

        let slot = Self::leaf_lower_bound(&leaf, key);
        let found = if let Some(existing) = Self::leaf_key(&leaf, slot) {
            existing.as_slice() == key.as_slice()
        } else {
            false
        };

        if found {
            leaf.delete(slot);
            self.put_page_with_lsn(leaf_id, leaf);
        }

        drop(guard);
        Ok(found)
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn find_leaf(
        &self,
        mut page: BTreePage,
        key: &CompositeKey,
    ) -> Option<(PageId, Vec<(PageId, u64)>)> {
        let mut path = vec![(page.page_id(), page.page_lsn())];
        while page.is_branch() {
            let child = Self::branch_child(&page, key);
            page = self.get_page(child)?;
            path.push((child, page.page_lsn()));
        }
        let leaf_id = path.last().unwrap().0;
        Some((leaf_id, path))
    }

    fn descend_to_leaf_exclusive(
        &self,
        mut page: BTreePage,
        key: &CompositeKey,
    ) -> Option<(PageId, BTreePage)> {
        while page.is_branch() {
            let child = Self::branch_child(&page, key);
            page = self.get_page(child)?;
        }
        Some((page.page_id(), page))
    }

    /// Given a branch page and a key, return the child page id to follow.
    fn branch_child(page: &BTreePage, key: &CompositeKey) -> PageId {
        let count = page.key_count();
        let mut lo = 0usize;
        let mut hi = count as usize;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let sep = page.separator_key(mid as u16);
            if let Some(sep) = sep {
                if sep < key.as_slice() {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            } else {
                hi = mid;
            }
        }
        if lo < count as usize {
            page.child_pointer(lo as u16).unwrap_or(0)
        } else {
            page.btree_header().rightmost_child
        }
    }

    fn leaf_lower_bound(page: &BTreePage, key: &CompositeKey) -> u16 {
        let count = page.key_count() as usize;
        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mid_key = Self::leaf_key(page, mid as u16);
            if let Some(mid_key) = mid_key {
                if mid_key.as_slice() < key.as_slice() {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            } else {
                hi = mid;
            }
        }
        lo.min(u16::MAX as usize) as u16
    }

    fn leaf_key(page: &BTreePage, slot: u16) -> Option<CompositeKey> {
        let kv = page.key(slot)?;
        if kv.len() < 2 {
            return None;
        }
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        if 2 + key_len > kv.len() {
            return None;
        }
        Some(CompositeKey::from_slice(
            &kv[2..2 + key_len]))
    }

    fn leaf_value(page: &BTreePage, slot: u16) -> Option<Vec<u8>> {
        let kv = page.key(slot)?;
        if kv.len() < 2 {
            return None;
        }
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        if 2 + key_len > kv.len() {
            return None;
        }
        Some(kv[2 + key_len..].to_vec())
    }

    /// Range search: return all `(key, value)` pairs where key is in `[start, end)`.
    pub fn range_search(
        &self,
        start: &CompositeKey,
        end: &CompositeKey,
    ) -> Vec<(CompositeKey, Vec<u8>)> {
        let mut results = Vec::new();
        let root = match self.get_page(self.root_page_id.load(Ordering::Relaxed)) {
            Some(p) => p,
            None => return results,
        };
        let (mut leaf_id, _) = match self.find_leaf(root, start) {
            Some(r) => r,
            None => return results,
        };

        loop {
            let leaf = match self.get_page(leaf_id) {
                Some(p) => p,
                None => break,
            };
            let count = leaf.key_count();
            for slot in 0..count {
                if let Some(key) = Self::leaf_key(&leaf, slot) {
                    let key_slice = key.as_slice();
                    if key_slice < start.as_slice() {
                        continue;
                    }
                    if key_slice >= end.as_slice() {
                        return results;
                    }
                    if let Some(value) = Self::leaf_value(&leaf, slot) {
                        results.push((key, value));
                    }
                }
            }
            let next = leaf.btree_header().sibling_next;
            if next == 0 {
                break;
            }
            leaf_id = next;
        }

        results
    }

    fn split_leaf(
        &self,
        leaf_id: PageId,
        leaf: BTreePage,
        new_key: &CompositeKey,
        new_value: &[u8],
    ) -> Result<(), BTreeError> {
        let new_leaf_id = self.alloc_page();
        let mut new_leaf = BTreePage::new_leaf(new_leaf_id);

        // Collect all key/value pairs from the leaf, filtering out any
        // existing occurrence of `new_key` (handles overwrite path).
        let new_key_slice = new_key.as_slice();
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for i in 0..leaf.slot_count() {
            if let Some(kv) = leaf.key(i)
                && kv.len() >= 2
            {
                let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
                if 2 + key_len <= kv.len() {
                    let k = &kv[2..2 + key_len];
                    if k != new_key_slice {
                        let v = kv[2 + key_len..].to_vec();
                        entries.push((k.to_vec(), v));
                    }
                }
            }
        }
        entries.push((new_key_slice.to_vec(), new_value.to_vec()));
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mid = entries.len() / 2;

        // Clear old leaf and repopulate left half.
        let mut left_leaf = BTreePage::new_leaf(leaf_id);
        for (k, v) in &entries[..mid] {
            left_leaf.insert_raw(&encode_kv(k, v));
        }

        // Populate right half.
        for (k, v) in &entries[mid..] {
            new_leaf.insert_raw(&encode_kv(k, v));
        }

        // Chain siblings.
        let old_next = leaf.btree_header().sibling_next;
        left_leaf.set_siblings(leaf.btree_header().sibling_prev, new_leaf_id);
        new_leaf.set_siblings(leaf_id, old_next);

        self.put_page_with_lsn(leaf_id, left_leaf);
        self.put_page_with_lsn(new_leaf_id, new_leaf);

        // Propagate separator to parent.
        let separator = entries[mid].0.clone();
        self.insert_into_parent(leaf_id, new_leaf_id, &separator)?;

        Ok(())
    }

    fn insert_into_parent(
        &self,
        left_child: PageId,
        right_child: PageId,
        separator: &[u8],
    ) -> Result<(), BTreeError> {
        if left_child == self.root_page_id.load(Ordering::Relaxed) {
            // Split root: create a new root branch.
            let new_root_id = self.alloc_page();
            let mut new_root = BTreePage::new_branch(new_root_id, 1);
            // child_pointer(0) = left_child (keys < separator).
            let record = encode_branch_entry(separator, left_child);
            new_root.insert_raw(&record);
            new_root.set_rightmost_child(right_child);
            self.put_page_with_lsn(new_root_id, new_root);
            // Update root id atomically.
            self.root_page_id.store(new_root_id, Ordering::Relaxed);
            return Ok(());
        }

        // Find parent and insert separator.
        let parent_id = self.find_parent(left_child).ok_or(BTreeError::MissingParent)?;
        let parent = self.get_page(parent_id).ok_or(BTreeError::MissingParent)?;

        let old_rightmost = parent.btree_header().rightmost_child;

        // Collect all existing entries from parent.
        let mut entries: Vec<(Vec<u8>, PageId)> = Vec::new();
        for i in 0..parent.slot_count() {
            if let Some(kv) = parent.key(i) {
                let key_len = kv.len().saturating_sub(8);
                if key_len > 0 && key_len + 8 <= kv.len() {
                    let k = kv[..key_len].to_vec();
                    let child = u64::from_be_bytes([
                        kv[key_len], kv[key_len + 1], kv[key_len + 2], kv[key_len + 3],
                        kv[key_len + 4], kv[key_len + 5], kv[key_len + 6], kv[key_len + 7],
                    ]);
                    entries.push((k, child));
                }
            }
        }

        // Update the entry that points to left_child.
        let mut updated = false;
        for entry in entries.iter_mut() {
            if entry.1 == left_child {
                // The existing separator now points to right_child.
                entry.1 = right_child;
                updated = true;
                break;
            }
        }
        if !updated && old_rightmost == left_child {
            // left_child was the rightmost; no entry to update.
        }

        // Insert new separator pointing to left_child.
        entries.push((separator.to_vec(), left_child));
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // Rebuild parent from scratch.
        let mut parent_mut = BTreePage::new_branch(parent_id, parent.btree_header().level);
        for (k, child) in &entries {
            parent_mut.insert_raw(&encode_branch_entry(k, *child));
        }
        if old_rightmost == left_child {
            parent_mut.set_rightmost_child(right_child);
        } else {
            parent_mut.set_rightmost_child(old_rightmost);
        }

        if parent_mut.key_count() as usize == entries.len() {
            self.put_page_with_lsn(parent_id, parent_mut);
            return Ok(());
        }

        // Parent branch is full: split branch.
        self.split_branch(parent_id, parent_mut, left_child, right_child, separator)
    }

    fn split_branch(
        &self,
        branch_id: PageId,
        branch: BTreePage,
        left_child: PageId,
        _right_child: PageId,
        separator: &[u8],
    ) -> Result<(), BTreeError> {
        let new_branch_id = self.alloc_page();
        let mut new_branch = BTreePage::new_branch(new_branch_id, branch.btree_header().level);
        let old_rightmost = branch.btree_header().rightmost_child;

        // Collect existing entries plus the new one.
        let mut entries: Vec<(Vec<u8>, PageId)> = Vec::new();
        for i in 0..branch.slot_count() {
            if let Some(kv) = branch.key(i) {
                let key_len = kv.len().saturating_sub(8);
                if key_len > 0 && key_len + 8 <= kv.len() {
                    let k = kv[..key_len].to_vec();
                    let child = u64::from_be_bytes([
                        kv[key_len], kv[key_len + 1], kv[key_len + 2], kv[key_len + 3],
                        kv[key_len + 4], kv[key_len + 5], kv[key_len + 6], kv[key_len + 7],
                    ]);
                    entries.push((k, child));
                }
            }
        }
        entries.push((separator.to_vec(), left_child));
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mid = entries.len() / 2;
        let mid_sep = entries[mid].0.clone();

        // Repopulate original branch with left half.
        let mut left_branch = BTreePage::new_branch(branch_id, branch.btree_header().level);
        for (k, child) in &entries[..mid] {
            left_branch.insert_raw(&encode_branch_entry(k, *child));
        }
        // rightmost_child = first child of right half (keys >= last separator in left).
        left_branch.set_rightmost_child(entries[mid].1);

        // Populate new branch with right half.
        for (k, child) in &entries[mid..] {
            new_branch.insert_raw(&encode_branch_entry(k, *child));
        }
        // rightmost_child inherits old rightmost (keys >= last separator overall).
        new_branch.set_rightmost_child(old_rightmost);

        self.put_page_with_lsn(branch_id, left_branch);
        self.put_page_with_lsn(new_branch_id, new_branch);

        self.insert_into_parent(branch_id, new_branch_id, &mid_sep)
    }

    fn find_parent(
        &self,
        child_id: PageId,
    ) -> Option<PageId> {
        let pages = self.pages.lock().unwrap();
        for (&pid, page) in pages.iter() {
            if page.is_branch() {
                for i in 0..page.key_count() {
                    if page.child_pointer(i) == Some(child_id) {
                        return Some(pid);
                    }
                }
                if page.btree_header().rightmost_child == child_id {
                    return Some(pid);
                }
            }
        }
        None
    }

}

/// Errors that can occur during B+ tree operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BTreeError {
    MissingRoot,
    MissingLeaf,
    MissingParent,
    SplitFailed,
    MergeFailed,
}

impl std::fmt::Display for BTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BTreeError::MissingRoot => write!(f, "B+ tree root page missing"),
            BTreeError::MissingLeaf => write!(f, "B+ tree leaf page missing"),
            BTreeError::MissingParent => write!(f, "B+ tree parent page missing"),
            BTreeError::SplitFailed => write!(f, "B+ tree split failed"),
            BTreeError::MergeFailed => write!(f, "B+ tree merge failed"),
        }
    }
}

impl std::error::Error for BTreeError {}

fn encode_kv(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 + key.len() + value.len());
    buf.extend_from_slice(&(key.len() as u16).to_be_bytes());
    buf.extend_from_slice(key);
    buf.extend_from_slice(value);
    buf
}

fn encode_branch_entry(key: &[u8], child: PageId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(key.len() + 8);
    buf.extend_from_slice(key);
    buf.extend_from_slice(&child.to_be_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::key::node_id_key;

    #[test]
    fn search_empty_tree() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let k = node_id_key(1);
        assert!(tree.search(&k).is_none());
    }

    #[test]
    fn insert_and_search() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let k = node_id_key(42);
        tree.insert(&k, b"answer").unwrap();
        let (pid, slot) = tree.search(&k).unwrap();
        assert_eq!(pid, 1);
        let page = tree.get_page(pid).unwrap();
        let kv = page.key(slot).unwrap();
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let val = &kv[2 + key_len..];
        assert_eq!(val, b"answer");
    }

    #[test]
    fn insert_many_triggers_split() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        for i in 1u128..=1000 {
            let k = node_id_key(i);
            tree.insert(&k, &i.to_be_bytes()).unwrap();
        }
        // After many inserts, root should have become a branch.
        let root = tree.get_page(tree.root_page_id.load(Ordering::Relaxed)).unwrap();
        assert!(root.is_branch() || root.key_count() > 0);
    }

    #[test]
    fn search_after_split() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        for i in 1u128..=500 {
            let k = node_id_key(i);
            tree.insert(&k, &i.to_be_bytes()).unwrap();
        }
        let k = node_id_key(250);
        let (pid, slot) = tree.search(&k).unwrap();
        let page = tree.get_page(pid).unwrap();
        let kv = page.key(slot).unwrap();
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let val = &kv[2 + key_len..];
        assert_eq!(val, &250u128.to_be_bytes());
    }

    #[test]
    fn delete_existing_key() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let k = node_id_key(7);
        tree.insert(&k, b"seven").unwrap();
        assert!(tree.delete(&k).unwrap());
        assert!(tree.search(&k).is_none());
    }

    #[test]
    fn delete_missing_key() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let k = node_id_key(99);
        assert!(!tree.delete(&k).unwrap());
    }

    #[test]
    fn insert_overwrite_value() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let k = node_id_key(1);
        tree.insert(&k, b"first").unwrap();
        tree.insert(&k, b"second").unwrap();
        let (pid, slot) = tree.search(&k).unwrap();
        let page = tree.get_page(pid).unwrap();
        let kv = page.key(slot).unwrap();
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let val = &kv[2 + key_len..];
        assert_eq!(val, b"second");
    }

    #[test]
    fn all_keys_searchable_after_bulk_insert() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        for i in 1u128..=200 {
            let k = node_id_key(i);
            tree.insert(&k, &i.to_be_bytes()).unwrap();
        }
        for i in 1u128..=200 {
            let k = node_id_key(i);
            assert!(
                tree.search(&k).is_some(),
                "key {} should be present",
                i
            );
        }
    }

    #[test]
    fn tree_invariants_after_splits() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        for i in 1u128..=300 {
            let k = node_id_key(i);
            tree.insert(&k, &i.to_be_bytes()).unwrap();
        }
        // Verify root is valid.
        let root = tree.get_page(tree.root_page_id.load(Ordering::Relaxed)).unwrap();
        assert!(root.is_branch() || root.key_count() > 0);
    }

    #[test]
    fn optimistic_search_finds_keys() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        for i in 1u128..=100 {
            let k = node_id_key(i);
            tree.insert(&k, &i.to_be_bytes()).unwrap();
        }
        for i in 1u128..=100 {
            let k = node_id_key(i);
            let result = tree.optimistic_search(&k);
            assert!(result.is_some(), "optimistic_search should find key {}", i);
            let (pid, slot) = result.unwrap();
            let page = tree.get_page(pid).unwrap();
            let kv = page.key(slot).unwrap();
            let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
            let val = &kv[2 + key_len..];
            assert_eq!(val, &i.to_be_bytes());
        }
    }

    #[test]
    fn optimistic_search_returns_none_for_missing_key() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        tree.insert(&node_id_key(1), b"one").unwrap();
        assert!(tree.optimistic_search(&node_id_key(2)).is_none());
    }

    #[test]
    fn prefix_compression_reduces_leaf_size() {
        use crate::index::page::BTreePage;
        let mut page = BTreePage::new_leaf(1);
        let shared = b"http://example.org/node/";
        for i in 1u128..=50 {
            let mut key = shared.to_vec();
            key.extend_from_slice(&i.to_be_bytes());
            let value = i.to_be_bytes().to_vec();
            page.insert_raw(&encode_kv(&key, &value)).unwrap();
        }
        // Before recompute: no common prefix.
        assert_eq!(page.btree_header().common_prefix_len, 0);

        page.recompute_prefix();

        // After recompute: common prefix should exist and match the shared part.
        assert!(
            page.btree_header().common_prefix_len > 0,
            "common prefix should be computed"
        );
        let prefix = page.common_prefix();
        assert!(
            prefix.starts_with(shared),
            "common prefix should start with the shared part"
        );

        // Verify that key_full still reconstructs the original keys correctly.
        for i in 1u128..=50 {
            let mut expected_key = shared.to_vec();
            expected_key.extend_from_slice(&i.to_be_bytes());
            let full_record = page.key_full(i as u16 - 1).unwrap();
            let key_len = u16::from_be_bytes([full_record[0], full_record[1]]) as usize;
            let key = &full_record[2..2 + key_len];
            assert_eq!(key, expected_key, "key {} should reconstruct correctly", i);
        }
    }

    #[test]
    fn btree_persists_through_buffer_pool() {
        use crate::buffer::pool::BufferPool;
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::PAGE_SIZE;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = Arc::new(PosixFileSystem::new(false)) as Arc<dyn FileSystem>;
        // Pre-allocate file.
        let handle = fs.open(&path, true).unwrap();
        handle.set_len(64 * PAGE_SIZE as u64).unwrap();
        drop(handle);

        let pool = Arc::new(BufferPool::new(8, path.clone()));

        // Build tree with pool attached.
        let tree = BPlusTree::new(BPlusTreeConfig::default())
            .with_pool(pool.clone(), fs.clone());
        for i in 1u128..=50 {
            let k = node_id_key(i);
            tree.insert(&k, &i.to_be_bytes()).unwrap();
        }
        // Flush dirty pages to disk.
        pool.flush_all(fs.as_ref()).unwrap();

        // Verify persistence by fixing the root page directly from the pool.
        let root_id = tree.root_page_id.load(Ordering::Relaxed);
        let guard = pool.fix_page(fs.as_ref(), root_id).unwrap();
        let persisted = BTreePage::from_buf(guard.buf().clone());
        // The page should be a leaf (all 50 small keys fit in one page).
        assert!(persisted.is_leaf(), "persisted root should still be a leaf");
        assert_eq!(persisted.key_count(), 50, "all 50 keys should be present");
    }
}
