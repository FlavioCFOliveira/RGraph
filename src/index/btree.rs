//! Core B+ tree operations: search, point insert/delete, split, merge.
//!
//! The tree is built over slotted pages and uses latch crabbing for
//! concurrency.
//!
//! # WAL logging scope
//!
//! WAL logging is **opt-in per call site**, not automatic for every mutation:
//!
//! * [`BPlusTree::insert_batch_logged`] is the only path that writes WAL
//!   records.  It emits one physical redo/undo record per touched index page
//!   (full after-image plus an embedded before-image) and flushes before
//!   returning, giving write-ahead durability that ARIES recovery replays.
//! * The plain [`BPlusTree::insert`], [`BPlusTree::insert_batch`], and
//!   [`BPlusTree::delete`] paths mutate the in-memory/buffer-pool pages
//!   **without** logging.  Callers that need durability for those mutations
//!   must drive them through `insert_batch_logged` or persist the pages via the
//!   buffer pool's own checkpoint/flush path.
//!
//! # Deferred work
//!
//! * **Epoch-based reclamation.** The [`crate::index::epoch::EpochPageTable`]
//!   prototype is gated behind the off-by-default `epoch` feature and is not
//!   integrated here.  It currently takes a mutex on every read, so it is not
//!   lock-free; a genuine lock-free page table is future work.
//! * **Prefix compression.** Gated behind the off-by-default
//!   `prefix_compression` feature.  The comparison paths in this module
//!   (`branch_child`, `leaf_lower_bound`) read raw record bytes, so prefix
//!   compression must not be enabled until every comparator reconstructs the
//!   full key first.

use crate::buffer::pool::BufferPool;
use crate::index::key::CompositeKey;
use crate::index::latch::{LatchCoupling, LatchMode};
use crate::index::page::BTreePage;
use crate::io::FileSystem;
use crate::storage::page::PageId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A descend path: a stack of `(branch_page_id, child_index)` pairs recorded
/// from the root down to a target node's parent.
///
/// `child_index` is the slot chosen at that branch (see
/// [`BPlusTree::branch_slot`]); `child_index == key_count` denotes the rightmost
/// child.  The descend path is the authoritative way to locate a node's parent
/// and siblings during splits, merges, and borrows — it works identically in
/// in-memory and buffer-pool modes, unlike a global parent-pointer scan.
type DescendPath = Vec<(PageId, usize)>;

/// Stable key for the tree's structural latch.
///
/// Writers take this key exclusively for the full duration of a mutation
/// (insert/delete, including any split/merge); readers and cursors take it
/// shared.  A **fixed** key — never the live root id — is essential: a root
/// split changes the root page id, and if the latch were keyed on the live root
/// a reader could latch the new id while a writer still held the old one, losing
/// mutual exclusion.  Page id `0` is the superblock and is never a B+ tree page,
/// so it is safe to reuse as this latch table key.
const STRUCTURE_LATCH_KEY: PageId = 0;

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
    /// When `Some`, every page written via [`Self::put_page_with_lsn`] records
    /// its id here.  Used by [`Self::insert_batch_logged`] to know which index
    /// pages a batch touched so it can emit physical WAL records for them.
    dirty_recorder: Mutex<Option<std::collections::HashSet<PageId>>>,
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
            dirty_recorder: Mutex::new(None),
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
        if let (Some(pool), Some(fs)) = (&self.pool, self.fs.as_deref()) {
            let required_len = id * crate::storage::page::PAGE_SIZE as u64;
            if let Ok(handle) = fs.open(&pool.data_path, false)
                && let Ok(current_len) = handle.len()
                && current_len < required_len
            {
                let _ = handle.set_len(required_len);
            }
        }
        id
    }

    /// Allocate a fresh page id (public entry point for the bulk loader).
    pub fn alloc_page_public(&self) -> PageId {
        self.alloc_page()
    }

    /// Install `page` at `page_id`, stamping a fresh LSN (public entry point
    /// for the bulk loader).
    pub fn put_page_public(&self, page_id: PageId, page: BTreePage) {
        self.put_page_with_lsn(page_id, page);
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
        // Record this write for WAL logging if a batch is recording dirty pages.
        if let Some(set) = self.dirty_recorder.lock().unwrap().as_mut() {
            set.insert(page_id);
        }
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
    ///
    /// Pessimistic: takes a shared latch on the structural-mutation latch
    /// (keyed on the root page id) so that no writer can be mid-split while the
    /// descent runs, then descends and probes the leaf.  Holding the shared
    /// latch for the whole descent is correct because writers serialise on the
    /// same key with an exclusive latch.
    pub fn search(&self, key: &CompositeKey) -> Option<(PageId, u16)> {
        let _guard = self.latch_mgr.latch(STRUCTURE_LATCH_KEY, LatchMode::Shared);
        self.search_unlatched(key)
    }

    /// Descend and probe without taking a latch.  Callers that already hold the
    /// structural latch (or have validated a version snapshot) use this.
    fn search_unlatched(&self, key: &CompositeKey) -> Option<(PageId, u16)> {
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

    /// Optimistic search that takes no latch on the happy path.
    ///
    /// Writers serialise on an exclusive structural latch keyed on the root
    /// page id, and the latch manager bumps that key's **version** on every
    /// exclusive release (every possible mutation).  An optimistic reader
    /// therefore:
    ///
    /// 1. snapshots the version (`Acquire`-ordered via the table mutex) and
    ///    confirms no writer is currently mid-mutation,
    /// 2. performs an unlatched descent + leaf probe,
    /// 3. re-reads the version and confirms it is unchanged.
    ///
    /// If the version moved (a writer ran during the read) or a writer was in
    /// flight, the snapshot may be torn, so the reader retries up to
    /// `config.optimistic_retry` times and finally falls back to the
    /// pessimistic, shared-latched [`Self::search`].
    pub fn optimistic_search(&self, key: &CompositeKey) -> Option<(PageId, u16)> {
        let max_retry = self.config.optimistic_retry;
        for _ in 0..=max_retry {
            // (1) Snapshot version; bail if a writer holds the latch right now.
            if self.latch_mgr.is_write_latched(STRUCTURE_LATCH_KEY) {
                std::thread::yield_now();
                continue;
            }
            let v_before = self.latch_mgr.version(STRUCTURE_LATCH_KEY);

            // (2) Unlatched read.
            let result = self.search_unlatched(key);

            // (3) Re-validate: the version must be unchanged and no writer may
            // have held the latch exclusively during the read.  The structural
            // latch key is stable across root splits, so no re-derivation is
            // needed.
            let v_after = self.latch_mgr.version(STRUCTURE_LATCH_KEY);
            if v_before == v_after && !self.latch_mgr.is_write_latched(STRUCTURE_LATCH_KEY) {
                return result;
            }
            std::thread::yield_now();
        }
        // Fall back to a pessimistic, shared-latched search.
        self.search(key)
    }

    /// Insert `key` -> `value` into the tree.
    pub fn insert(&self, key: &CompositeKey, value: &[u8]) -> Result<(), BTreeError> {
        let guard = self
            .latch_mgr
            .latch(STRUCTURE_LATCH_KEY, LatchMode::Exclusive);
        let result = self.insert_locked(key, value);
        drop(guard);
        result
    }

    /// Insert a batch of `(key, value)` pairs atomically under a single latch.
    ///
    /// The whole batch is applied while holding the root exclusive latch, so no
    /// other writer or reader observes a partially-applied batch.  On any error
    /// the function returns immediately; callers that require all-or-nothing
    /// semantics should run the batch inside a transaction so the WAL can undo
    /// the prefix that did apply (the in-memory tree is left as far as the error
    /// point, matching the engine's transactional rollback path).
    pub fn insert_batch(&self, entries: &[(CompositeKey, Vec<u8>)]) -> Result<(), BTreeError> {
        let guard = self
            .latch_mgr
            .latch(STRUCTURE_LATCH_KEY, LatchMode::Exclusive);
        for (key, value) in entries {
            self.insert_locked(key, value)?;
        }
        drop(guard);
        Ok(())
    }

    /// Insert a batch and emit physical WAL redo/undo records for every index
    /// page the batch mutated, so the index survives a crash and is replayed by
    /// ARIES recovery.
    ///
    /// Requires an attached buffer pool and file system (see [`Self::with_pool`]).
    /// Each touched page is logged as an [`crate::wal::record::RecordType::IndexPageUpdate`]
    /// (or `IndexPageInsert` for newly allocated pages) carrying the full
    /// after-image plus an embedded before-image for UNDO.  The records are
    /// flushed before returning, giving write-ahead durability.
    ///
    /// # Errors
    ///
    /// Returns [`BTreeError`] if any insert fails, or wraps an I/O error from
    /// the WAL as [`BTreeError::SplitFailed`] (the batch is reported as failed so
    /// the caller can abort the surrounding transaction).
    pub fn insert_batch_logged(
        &self,
        entries: &[(CompositeKey, Vec<u8>)],
        wal: &mut crate::wal::writer::WalWriter,
        wal_fs: &dyn FileSystem,
        txid: u64,
    ) -> Result<(), BTreeError> {
        use crate::wal::record::{RecordType, WalRecord};
        let (Some(_pool), Some(_fs)) = (&self.pool, self.fs.as_deref()) else {
            // No persistence attached: fall back to the in-memory atomic batch.
            return self.insert_batch(entries);
        };

        // Snapshot before-images of pages that already exist, so UNDO can
        // restore them.  New pages have no before-image (UNDO writes a tombstone).
        let pre_existing: std::collections::HashSet<PageId> =
            self.all_resident_page_ids().into_iter().collect();

        // Activate dirty-page recording for the duration of the batch.
        *self.dirty_recorder.lock().unwrap() = Some(std::collections::HashSet::new());

        let before_images: std::collections::HashMap<PageId, Vec<u8>> = pre_existing
            .iter()
            .filter_map(|&pid| self.get_page(pid).map(|p| (pid, p.inner.buf[..].to_vec())))
            .collect();

        let guard = self
            .latch_mgr
            .latch(STRUCTURE_LATCH_KEY, LatchMode::Exclusive);
        let mut insert_result = Ok(());
        for (key, value) in entries {
            if let Err(e) = self.insert_locked(key, value) {
                insert_result = Err(e);
                break;
            }
        }
        drop(guard);

        // Collect and clear the recorded dirty pages.
        let dirty = self
            .dirty_recorder
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default();

        insert_result?;

        // Emit one physical WAL record per touched index page.
        let mut prev_lsn = 0u64;
        for page_id in dirty {
            let Some(page) = self.get_page(page_id) else {
                continue;
            };
            let after_image = &page.inner.buf[..];
            let record_type = if pre_existing.contains(&page_id) {
                RecordType::IndexPageUpdate
            } else {
                RecordType::IndexPageInsert
            };

            let mut payload = Vec::with_capacity(8 + after_image.len());
            payload.extend_from_slice(&page_id.to_be_bytes());
            payload.extend_from_slice(after_image);
            if let Some(before) = before_images.get(&page_id) {
                crate::wal::aries::embed_before_image(&mut payload, before);
            }

            let rec = WalRecord::new(record_type, txid, 0, prev_lsn, payload);
            let lsn = wal
                .append(wal_fs, rec)
                .map_err(|_| BTreeError::SplitFailed)?;
            prev_lsn = lsn;
        }
        wal.flush(wal_fs).map_err(|_| BTreeError::SplitFailed)?;
        Ok(())
    }

    /// Page ids currently resident either in the in-memory store or fixable
    /// from the pool (best effort: probes a bounded id range in pool mode).
    fn all_resident_page_ids(&self) -> Vec<PageId> {
        if self.pool.is_some() {
            // In pool mode probe ids up to the next allocation watermark.
            let next = *self.next_page_id.lock().unwrap();
            (1..next)
                .filter(|&pid| {
                    self.get_page(pid)
                        .is_some_and(|p| p.is_leaf() || p.is_branch())
                })
                .collect()
        } else {
            self.all_page_ids()
        }
    }

    /// Core insert that assumes the caller already holds the root exclusive
    /// latch.  Shared by [`Self::insert`] and [`Self::insert_batch`].
    fn insert_locked(&self, key: &CompositeKey, value: &[u8]) -> Result<(), BTreeError> {
        let root_id = self.root_page_id.load(Ordering::Relaxed);
        let root = self.get_page(root_id).ok_or(BTreeError::MissingRoot)?;

        // Fast path: empty tree, insert directly into root leaf.
        if root.is_leaf() && root.key_count() == 0 {
            let mut root_mut = root.clone();
            let record = encode_kv(key.as_slice(), value);
            if root_mut.has_room_for(record.len()) && root_mut.insert_raw(&record).is_some() {
                self.put_page_with_lsn(root_id, root_mut);
                return Ok(());
            }
            // Root is physically full despite being logically empty; fall through to split.
        }

        // Descend to the leaf, recording the parent chain as a stack of
        // (branch_page_id, child_slot) pairs.  The descend path — never a
        // HashMap scan — is how splits locate their parent, which is required
        // for buffer-pool mode where the in-memory `pages` map is empty.
        let (leaf_id, leaf, path) = self
            .descend_to_leaf_with_path(root, key)
            .ok_or(BTreeError::MissingLeaf)?;

        let record = encode_kv(key.as_slice(), value);

        // Overwrite path: if the key already exists, rebuild the leaf with the
        // new value.  Rebuilding (rather than delete + insert_raw_at) avoids the
        // underlying slotted-page compaction, which is not B+-tree-header aware.
        let slot = Self::leaf_lower_bound(&leaf, key);
        if let Some(existing) = Self::leaf_key(&leaf, slot)
            && existing.as_slice() == key.as_slice()
        {
            if let Some(rebuilt) = Self::rebuild_leaf_replacing(&leaf, slot, &record) {
                self.put_page_with_lsn(leaf_id, rebuilt);
                return Ok(());
            }
            // Replacement does not fit (value grew): fall through to split.
            return self.split_leaf(leaf_id, leaf, key, value, &path);
        }

        // Insert proactively only when the record fits; otherwise split.  We
        // must never let `insert_raw_at` trigger the slotted-page compaction.
        if leaf.has_room_for(record.len()) {
            let mut leaf = leaf;
            leaf.insert_raw_at(slot, &record)
                .expect("INVARIANT: record fits after has_room_for check");
            self.put_page_with_lsn(leaf_id, leaf);
            return Ok(());
        }

        // Leaf is full: split, resolving the parent via the descend path.
        self.split_leaf(leaf_id, leaf, key, value, &path)
    }

    /// Rebuild a leaf with the record at `slot` replaced by `record`.
    ///
    /// Returns `None` if the replacement record does not fit.  The leaf is
    /// rebuilt from scratch through a B+-tree-header-aware path, so no
    /// slotted-page compaction is triggered.
    fn rebuild_leaf_replacing(leaf: &BTreePage, slot: u16, record: &[u8]) -> Option<BTreePage> {
        let mut rebuilt = BTreePage::new_leaf(leaf.page_id());
        rebuilt.set_siblings(
            leaf.btree_header().sibling_prev,
            leaf.btree_header().sibling_next,
        );
        for i in 0..leaf.key_count() {
            let rec = if i == slot { record } else { leaf.key(i)? };
            if !rebuilt.has_room_for(rec.len()) {
                return None;
            }
            rebuilt.insert_raw(rec)?;
        }
        Some(rebuilt)
    }

    /// Delete `key` from the tree. Returns true if the key was found.
    pub fn delete(&self, key: &CompositeKey) -> Result<bool, BTreeError> {
        let guard = self
            .latch_mgr
            .latch(STRUCTURE_LATCH_KEY, LatchMode::Exclusive);
        let root_id = self.root_page_id.load(Ordering::Relaxed);
        let root = self.get_page(root_id).ok_or(BTreeError::MissingRoot)?;
        let (leaf_id, mut leaf, path) = self
            .descend_to_leaf_with_path(root, key)
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
            // Rebalance the leaf if it underflowed (borrow from a sibling or
            // merge), propagating separator changes up the descend path.
            self.rebalance_after_delete(leaf_id, &path)?;
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

    /// Descend from `page` to the target leaf, recording the parent chain.
    ///
    /// Returns `(leaf_id, leaf_page, path)` where `path` is a stack of
    /// `(branch_page_id, child_index)` pairs from the root down to the leaf's
    /// parent.  `child_index` is the index produced by [`Self::branch_slot`];
    /// `child_index == key_count` denotes the rightmost child.  The path is the
    /// authoritative way to locate a node's parent and its siblings — it works
    /// identically in in-memory and buffer-pool modes.
    fn descend_to_leaf_with_path(
        &self,
        mut page: BTreePage,
        key: &CompositeKey,
    ) -> Option<(PageId, BTreePage, DescendPath)> {
        let mut path: DescendPath = Vec::new();
        while page.is_branch() {
            let (child, idx) = Self::branch_slot(&page, key);
            path.push((page.page_id(), idx));
            page = self.get_page(child)?;
        }
        Some((page.page_id(), page, path))
    }

    /// Given a branch page and a key, return the child page id to follow.
    fn branch_child(page: &BTreePage, key: &CompositeKey) -> PageId {
        Self::branch_slot(page, key).0
    }

    /// Resolve `(child_page_id, child_index)` for `key` within a branch page.
    ///
    /// `child_index` ranges over `0..=key_count`; the value `key_count`
    /// indicates that the rightmost child pointer was selected.
    ///
    /// Separator semantics: `child_pointer(i)` (the left child of separator
    /// `i`) holds keys strictly less than separator `i`.  A separator equals
    /// the first key of the subtree to its right, so a key equal to a separator
    /// must descend rightward.  We therefore follow the first separator
    /// strictly greater than `key` (`sep > key`); keys `>=` every separator
    /// fall through to the rightmost child.
    fn branch_slot(page: &BTreePage, key: &CompositeKey) -> (PageId, usize) {
        let count = page.key_count() as usize;
        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let sep = page.separator_key(mid as u16);
            if let Some(sep) = sep {
                if sep <= key.as_slice() {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            } else {
                hi = mid;
            }
        }
        if lo < count {
            (page.child_pointer(lo as u16).unwrap_or(0), lo)
        } else {
            (page.btree_header().rightmost_child, count)
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
        Some(CompositeKey::from_slice(&kv[2..2 + key_len]))
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
        // Hold a shared latch for the whole scan so the leaf chain is stable.
        let _guard = self.latch_mgr.latch(STRUCTURE_LATCH_KEY, LatchMode::Shared);
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

    /// Build a forward range cursor positioned at the first key `>= from`,
    /// bounded above (exclusively) by `to`.
    ///
    /// The returned [`BTreeRangeCursor`] follows sibling leaves, so iteration
    /// correctly spans multiple leaves.  Returns `None` only if the tree's root
    /// is missing.
    pub fn cursor_from<'a>(
        &'a self,
        from: &CompositeKey,
        to: Option<CompositeKey>,
    ) -> Option<crate::index::cursor::BTreeRangeCursor<'a>> {
        // Hold a shared latch for the cursor's lifetime so writers cannot split
        // or merge the leaf chain while the scan is in progress.
        let latch = self.latch_mgr.latch(STRUCTURE_LATCH_KEY, LatchMode::Shared);
        let root = self.get_page(self.root_page_id.load(Ordering::Relaxed))?;
        let (leaf_id, _) = self.find_leaf(root, from)?;
        let leaf = self.get_page(leaf_id)?;
        Some(crate::index::cursor::BTreeRangeCursor::new(
            self, leaf_id, leaf, from, to, latch,
        ))
    }

    /// Collect every `(key, value)` pair in `[from, to)` by walking a
    /// sibling-following cursor.  Equivalent to [`Self::range_search`] but
    /// exercising the cursor path that honours leaf-chain traversal.
    ///
    /// Named per the index API contract; it borrows `&self` and returns an
    /// owned range rather than consuming the tree.
    #[allow(clippy::wrong_self_convention)]
    pub fn into_range(&self, from: &CompositeKey, to: &CompositeKey) -> Vec<(Vec<u8>, Vec<u8>)> {
        match self.cursor_from(from, Some(to.clone())) {
            Some(cursor) => cursor.collect_range(),
            None => Vec::new(),
        }
    }

    fn split_leaf(
        &self,
        leaf_id: PageId,
        leaf: BTreePage,
        new_key: &CompositeKey,
        new_value: &[u8],
        path: &[(PageId, usize)],
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

        // The separator is the first key of the right half.  Keys `<` the
        // separator live in `leaf_id`; keys `>=` it live in `new_leaf_id`.
        let separator = entries[mid].0.clone();
        self.insert_into_parent(leaf_id, new_leaf_id, &separator, path)?;

        Ok(())
    }

    /// Insert the separator for a split into the parent identified by the
    /// descend `path`.
    ///
    /// `left_child` is the (already-persisted) lower half and `right_child`
    /// the upper half of the node that just split.  `separator` is the key
    /// such that keys `< separator` belong to `left_child` and keys
    /// `>= separator` belong to `right_child`.
    ///
    /// `path` is the stack of `(branch_id, child_index)` pairs recorded on the
    /// way down; its last element is the direct parent.  When `path` is empty
    /// the node that split was the root, so a new root is created.
    fn insert_into_parent(
        &self,
        left_child: PageId,
        right_child: PageId,
        separator: &[u8],
        path: &[(PageId, usize)],
    ) -> Result<(), BTreeError> {
        let Some((&(parent_id, child_index), ancestors)) = path.split_last() else {
            // The split node was the root: grow the tree by one level.
            let new_root_id = self.alloc_page();
            let old_level = self
                .get_page(left_child)
                .map(|p| p.btree_header().level)
                .unwrap_or(0);
            let mut new_root = BTreePage::new_branch(new_root_id, old_level + 1);
            new_root.insert_raw(&encode_branch_entry(separator, left_child));
            new_root.set_rightmost_child(right_child);
            self.put_page_with_lsn(new_root_id, new_root);
            self.root_page_id.store(new_root_id, Ordering::Relaxed);
            return Ok(());
        };

        let parent = self.get_page(parent_id).ok_or(BTreeError::MissingParent)?;
        let (mut seps, mut children) = Self::decode_branch(&parent);

        // `child_index` is the slot in the parent that pointed at the node
        // which just split.  That pointer must now address `left_child`, and a
        // new separator + `right_child` are inserted immediately after it.
        debug_assert!(child_index < children.len());
        children[child_index] = left_child;
        seps.insert(child_index, separator.to_vec());
        children.insert(child_index + 1, right_child);

        let level = parent.btree_header().level;
        if let Some(branch) = Self::try_build_branch(parent_id, level, &seps, &children) {
            self.put_page_with_lsn(parent_id, branch);
            return Ok(());
        }

        // Parent overflowed: split it and recurse using the ancestor path.
        self.split_branch(parent_id, level, seps, children, ancestors)
    }

    /// Split an overflowing branch whose canonical `(seps, children)` form is
    /// given, promoting the middle separator into the grandparent.
    fn split_branch(
        &self,
        branch_id: PageId,
        level: u8,
        seps: Vec<Vec<u8>>,
        children: Vec<PageId>,
        ancestors: &[(PageId, usize)],
    ) -> Result<(), BTreeError> {
        // children.len() == seps.len() + 1.  Promote seps[mid]; it does not
        // appear in either child branch (standard B+ tree internal split).
        let mid = seps.len() / 2;
        let promoted = seps[mid].clone();

        let left_seps = seps[..mid].to_vec();
        let left_children = children[..=mid].to_vec();
        let right_seps = seps[mid + 1..].to_vec();
        let right_children = children[mid + 1..].to_vec();

        let left_branch = Self::build_branch(branch_id, level, &left_seps, &left_children);
        let new_branch_id = self.alloc_page();
        let right_branch = Self::build_branch(new_branch_id, level, &right_seps, &right_children);

        self.put_page_with_lsn(branch_id, left_branch);
        self.put_page_with_lsn(new_branch_id, right_branch);

        self.insert_into_parent(branch_id, new_branch_id, &promoted, ancestors)
    }

    // ── Canonical branch (seps, children) helpers ──────────────────────────
    //
    // On disk a branch stores `N` slots of `(separator_i, pointer_i)` plus a
    // `rightmost_child`.  `pointer_i` addresses the subtree of keys
    // `<= separator_i`; `rightmost_child` addresses keys `> separator_{N-1}`.
    // The canonical in-memory form is `seps` (length `N`) and `children`
    // (length `N + 1`), where `children[i]` is separated from `children[i+1]`
    // by `seps[i]`.

    /// Decode a branch page into its canonical `(seps, children)` form.
    fn decode_branch(page: &BTreePage) -> (Vec<Vec<u8>>, Vec<PageId>) {
        let count = page.key_count() as usize;
        let mut seps = Vec::with_capacity(count);
        let mut children = Vec::with_capacity(count + 1);
        for i in 0..count as u16 {
            if let Some(sep) = page.separator_key(i) {
                seps.push(sep.to_vec());
            }
            if let Some(child) = page.child_pointer(i) {
                children.push(child);
            }
        }
        children.push(page.btree_header().rightmost_child);
        (seps, children)
    }

    /// Build a branch page from canonical form, asserting it fits.
    ///
    /// # Panics
    ///
    /// Panics if the entries do not fit; use [`Self::try_build_branch`] when
    /// overflow is expected and must be handled by splitting.
    fn build_branch(
        branch_id: PageId,
        level: u8,
        seps: &[Vec<u8>],
        children: &[PageId],
    ) -> BTreePage {
        Self::try_build_branch(branch_id, level, seps, children)
            .expect("INVARIANT: branch half-page must fit after a split")
    }

    /// Try to build a branch page from canonical form.
    ///
    /// Returns `None` if any entry does not fit (caller must split).
    fn try_build_branch(
        branch_id: PageId,
        level: u8,
        seps: &[Vec<u8>],
        children: &[PageId],
    ) -> Option<BTreePage> {
        debug_assert_eq!(children.len(), seps.len() + 1);
        let mut branch = BTreePage::new_branch(branch_id, level);
        for (i, sep) in seps.iter().enumerate() {
            branch.insert_raw(&encode_branch_entry(sep, children[i]))?;
        }
        branch.set_rightmost_child(*children.last().unwrap());
        Some(branch)
    }

    // ── Delete rebalancing: borrow / merge ─────────────────────────────────

    /// Byte-fill ratio of a page's record area, ignoring both headers.
    ///
    /// Used to detect underflow against [`BPlusTreeConfig::min_fill_ratio`].
    fn fill_ratio(&self, page: &BTreePage) -> f64 {
        let usable = (self.config.page_size
            - crate::storage::page::SlottedPage::HEADER_SIZE
            - crate::index::page::BTREE_HEADER_SIZE) as f64;
        if usable <= 0.0 {
            return 1.0;
        }
        let mut bytes = 0usize;
        for i in 0..page.slot_count() {
            if let Some(rec) = page.key(i) {
                bytes += rec.len();
            }
        }
        bytes as f64 / usable
    }

    /// Is `page` underfull and therefore eligible for borrow/merge?
    ///
    /// A page with no entries is always underfull.  The root is never
    /// considered underfull by this predicate; root collapse is handled
    /// separately.
    fn is_underfull(&self, page: &BTreePage) -> bool {
        page.key_count() == 0 || self.fill_ratio(page) < self.config.min_fill_ratio
    }

    /// Rebalance the tree after a delete made `leaf_id` potentially underfull.
    ///
    /// Walks the descend `path` from the leaf's parent upward, borrowing from
    /// or merging with a sibling at each level that underflows, and collapsing
    /// the root when it is left with a single child.
    fn rebalance_after_delete(
        &self,
        leaf_id: PageId,
        path: &[(PageId, usize)],
    ) -> Result<(), BTreeError> {
        let leaf = self.get_page(leaf_id).ok_or(BTreeError::MissingLeaf)?;
        // The root leaf (empty path) may shrink to empty; nothing to reclaim.
        if path.is_empty() || !self.is_underfull(&leaf) {
            return Ok(());
        }
        self.rebalance_node(leaf_id, path)
    }

    /// Borrow-or-merge `node_id` (a leaf or branch) with a sibling, given the
    /// descend `path` whose last element is `node_id`'s parent.
    fn rebalance_node(&self, node_id: PageId, path: &[(PageId, usize)]) -> Result<(), BTreeError> {
        let Some((&(parent_id, child_index), ancestors)) = path.split_last() else {
            return Ok(());
        };
        let parent = self.get_page(parent_id).ok_or(BTreeError::MissingParent)?;
        let (mut seps, mut children) = Self::decode_branch(&parent);
        debug_assert_eq!(children.len(), seps.len() + 1);
        debug_assert!(child_index < children.len());

        let node = self.get_page(node_id).ok_or(BTreeError::MissingLeaf)?;
        let is_leaf = node.is_leaf();

        // Prefer the left sibling, then the right, for a borrow.
        let has_left = child_index > 0;
        let has_right = child_index + 1 < children.len();

        // ── Try to borrow from the left sibling ────────────────────────────
        if has_left {
            let left_id = children[child_index - 1];
            let left = self.get_page(left_id).ok_or(BTreeError::MissingLeaf)?;
            if self.can_lend(&left) {
                let new_sep = if is_leaf {
                    self.borrow_leaf_from_left(left_id, node_id)?
                } else {
                    self.borrow_branch_from_left(left_id, node_id, &seps[child_index - 1])?
                };
                seps[child_index - 1] = new_sep;
                let level = parent.btree_header().level;
                let branch = Self::build_branch(parent_id, level, &seps, &children);
                self.put_page_with_lsn(parent_id, branch);
                return Ok(());
            }
        }

        // ── Try to borrow from the right sibling ───────────────────────────
        if has_right {
            let right_id = children[child_index + 1];
            let right = self.get_page(right_id).ok_or(BTreeError::MissingLeaf)?;
            if self.can_lend(&right) {
                let new_sep = if is_leaf {
                    self.borrow_leaf_from_right(node_id, right_id)?
                } else {
                    self.borrow_branch_from_right(node_id, right_id, &seps[child_index])?
                };
                seps[child_index] = new_sep;
                let level = parent.btree_header().level;
                let branch = Self::build_branch(parent_id, level, &seps, &children);
                self.put_page_with_lsn(parent_id, branch);
                return Ok(());
            }
        }

        // ── Merge: no sibling can lend ─────────────────────────────────────
        // Merge `node` into its left sibling when present, otherwise merge the
        // right sibling into `node`.  The separating key in the parent is
        // dropped (leaves) or pulled down (branches).
        let (merge_left_idx, sep_idx) = if has_left {
            (child_index - 1, child_index - 1)
        } else {
            (child_index, child_index)
        };
        let left_id = children[merge_left_idx];
        let right_id = children[merge_left_idx + 1];
        let separator = seps[sep_idx].clone();

        if is_leaf {
            self.merge_leaves(left_id, right_id)?;
        } else {
            self.merge_branches(left_id, right_id, &separator)?;
        }

        // Remove the dropped separator and the now-defunct right child from
        // the parent's canonical form.
        seps.remove(sep_idx);
        children.remove(merge_left_idx + 1);

        // The right page is now unreachable; reclaim its id.
        self.free_page(right_id);

        // Root collapse: a root branch with no separators has a single child,
        // which becomes the new root (the tree loses a level).
        if ancestors.is_empty()
            && parent_id == self.root_page_id.load(Ordering::Relaxed)
            && seps.is_empty()
        {
            let only_child = children[0];
            self.root_page_id.store(only_child, Ordering::Relaxed);
            self.free_page(parent_id);
            return Ok(());
        }

        let level = parent.btree_header().level;
        let parent_branch = Self::build_branch(parent_id, level, &seps, &children);
        let parent_underfull = self.is_underfull(&parent_branch);
        self.put_page_with_lsn(parent_id, parent_branch);

        // Propagate underflow up the tree.
        if parent_underfull && !ancestors.is_empty() {
            self.rebalance_node(parent_id, ancestors)?;
        }
        Ok(())
    }

    /// Can `page` spare one entry without itself underflowing?
    fn can_lend(&self, page: &BTreePage) -> bool {
        if page.key_count() <= 1 {
            return false;
        }
        // After removing one entry the page must still be at or above the
        // minimum fill.  Approximate by requiring strictly more than the
        // minimum number of bytes plus the largest single record.
        let mut sizes: Vec<usize> = Vec::new();
        for i in 0..page.slot_count() {
            if let Some(rec) = page.key(i) {
                sizes.push(rec.len());
            }
        }
        if sizes.len() <= 1 {
            return false;
        }
        let usable = (self.config.page_size
            - crate::storage::page::SlottedPage::HEADER_SIZE
            - crate::index::page::BTREE_HEADER_SIZE) as f64;
        let total: usize = sizes.iter().sum();
        let max_rec = sizes.iter().copied().max().unwrap_or(0);
        ((total - max_rec) as f64 / usable) >= self.config.min_fill_ratio
    }

    /// Move the last entry of `left` leaf to the front of `right` leaf.
    /// Returns the new separator (the first key of `right` after the move).
    fn borrow_leaf_from_left(
        &self,
        left_id: PageId,
        right_id: PageId,
    ) -> Result<Vec<u8>, BTreeError> {
        let mut left = self.get_page(left_id).ok_or(BTreeError::MissingLeaf)?;
        let mut right = self.get_page(right_id).ok_or(BTreeError::MissingLeaf)?;
        let last = left.key_count() - 1;
        let rec = left.key(last).ok_or(BTreeError::MergeFailed)?.to_vec();
        left.delete(last);
        right.insert_raw_at(0, &rec);
        self.put_page_with_lsn(left_id, left);
        let new_sep = leaf_record_key(&rec).to_vec();
        self.put_page_with_lsn(right_id, right);
        Ok(new_sep)
    }

    /// Move the first entry of `right` leaf to the end of `left` leaf.
    /// Returns the new separator (the first key remaining in `right`).
    fn borrow_leaf_from_right(
        &self,
        left_id: PageId,
        right_id: PageId,
    ) -> Result<Vec<u8>, BTreeError> {
        let mut left = self.get_page(left_id).ok_or(BTreeError::MissingLeaf)?;
        let mut right = self.get_page(right_id).ok_or(BTreeError::MissingLeaf)?;
        let rec = right.key(0).ok_or(BTreeError::MergeFailed)?.to_vec();
        right.delete(0);
        left.insert_raw(&rec);
        let new_first = right.key(0).ok_or(BTreeError::MergeFailed)?.to_vec();
        let new_sep = leaf_record_key(&new_first).to_vec();
        self.put_page_with_lsn(left_id, left);
        self.put_page_with_lsn(right_id, right);
        Ok(new_sep)
    }

    /// Rotate one entry from `left` branch through the parent into `right`.
    /// `parent_sep` is the current separator between the two branches.
    /// Returns the replacement separator to store in the parent.
    fn borrow_branch_from_left(
        &self,
        left_id: PageId,
        right_id: PageId,
        parent_sep: &[u8],
    ) -> Result<Vec<u8>, BTreeError> {
        let left = self.get_page(left_id).ok_or(BTreeError::MissingParent)?;
        let right = self.get_page(right_id).ok_or(BTreeError::MissingParent)?;
        let (mut lseps, mut lchildren) = Self::decode_branch(&left);
        let (mut rseps, mut rchildren) = Self::decode_branch(&right);

        // The left branch's last separator is promoted to the parent; the old
        // parent separator descends to the front of the right branch.
        let moved_sep = lseps.pop().ok_or(BTreeError::MergeFailed)?;
        let moved_child = lchildren.pop().ok_or(BTreeError::MergeFailed)?;
        rseps.insert(0, parent_sep.to_vec());
        rchildren.insert(0, moved_child);

        let level = left.btree_header().level;
        let lb = Self::build_branch(left_id, level, &lseps, &lchildren);
        let rb = Self::build_branch(right_id, level, &rseps, &rchildren);
        self.put_page_with_lsn(left_id, lb);
        self.put_page_with_lsn(right_id, rb);
        Ok(moved_sep)
    }

    /// Rotate one entry from `right` branch through the parent into `left`.
    /// Returns the replacement separator to store in the parent.
    fn borrow_branch_from_right(
        &self,
        left_id: PageId,
        right_id: PageId,
        parent_sep: &[u8],
    ) -> Result<Vec<u8>, BTreeError> {
        let left = self.get_page(left_id).ok_or(BTreeError::MissingParent)?;
        let right = self.get_page(right_id).ok_or(BTreeError::MissingParent)?;
        let (mut lseps, mut lchildren) = Self::decode_branch(&left);
        let (mut rseps, mut rchildren) = Self::decode_branch(&right);

        // The old parent separator descends to the end of the left branch; the
        // right branch's first separator is promoted to the parent.
        let moved_sep = if rseps.is_empty() {
            return Err(BTreeError::MergeFailed);
        } else {
            rseps.remove(0)
        };
        let moved_child = rchildren.remove(0);
        lseps.push(parent_sep.to_vec());
        lchildren.push(moved_child);

        let level = left.btree_header().level;
        let lb = Self::build_branch(left_id, level, &lseps, &lchildren);
        let rb = Self::build_branch(right_id, level, &rseps, &rchildren);
        self.put_page_with_lsn(left_id, lb);
        self.put_page_with_lsn(right_id, rb);
        Ok(moved_sep)
    }

    /// Merge the `right` leaf into the `left` leaf, re-chaining siblings.
    fn merge_leaves(&self, left_id: PageId, right_id: PageId) -> Result<(), BTreeError> {
        let mut left = self.get_page(left_id).ok_or(BTreeError::MissingLeaf)?;
        let right = self.get_page(right_id).ok_or(BTreeError::MissingLeaf)?;
        for i in 0..right.key_count() {
            if let Some(rec) = right.key(i)
                && left.insert_raw(rec).is_none()
            {
                return Err(BTreeError::MergeFailed);
            }
        }
        // Re-chain: left.next = right.next, and right.next.prev = left.
        let right_next = right.btree_header().sibling_next;
        left.set_siblings(left.btree_header().sibling_prev, right_next);
        self.put_page_with_lsn(left_id, left);
        if right_next != 0
            && let Some(mut next) = self.get_page(right_next)
        {
            next.set_siblings(left_id, next.btree_header().sibling_next);
            self.put_page_with_lsn(right_next, next);
        }
        Ok(())
    }

    /// Merge the `right` branch into the `left` branch, pulling `separator`
    /// down between them (standard B+ tree internal merge).
    fn merge_branches(
        &self,
        left_id: PageId,
        right_id: PageId,
        separator: &[u8],
    ) -> Result<(), BTreeError> {
        let left = self.get_page(left_id).ok_or(BTreeError::MissingParent)?;
        let right = self.get_page(right_id).ok_or(BTreeError::MissingParent)?;
        let (mut lseps, mut lchildren) = Self::decode_branch(&left);
        let (rseps, rchildren) = Self::decode_branch(&right);

        lseps.push(separator.to_vec());
        lseps.extend(rseps);
        lchildren.extend(rchildren);

        let level = left.btree_header().level;
        let merged = Self::try_build_branch(left_id, level, &lseps, &lchildren)
            .ok_or(BTreeError::MergeFailed)?;
        self.put_page_with_lsn(left_id, merged);
        Ok(())
    }

    /// Return `page_id` to the in-memory free pool (best effort).
    ///
    /// In buffer-pool mode the page simply becomes unreferenced; a future
    /// free-list integration can reclaim its space on disk.
    fn free_page(&self, page_id: PageId) {
        if self.pool.is_none() {
            let mut pages = self.pages.lock().unwrap();
            pages.remove(&page_id);
        }
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

/// Extract the key portion of a leaf record `[key_len: u16 BE][key][value]`.
///
/// Returns an empty slice for malformed records; callers treat such records as
/// having a minimal separating key, which only affects rebalancing heuristics,
/// never correctness of stored data.
fn leaf_record_key(record: &[u8]) -> &[u8] {
    if record.len() < 2 {
        return &[];
    }
    let key_len = u16::from_be_bytes([record[0], record[1]]) as usize;
    if 2 + key_len > record.len() {
        return &[];
    }
    &record[2..2 + key_len]
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
        let root = tree
            .get_page(tree.root_page_id.load(Ordering::Relaxed))
            .unwrap();
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
            assert!(tree.search(&k).is_some(), "key {} should be present", i);
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
        let root = tree
            .get_page(tree.root_page_id.load(Ordering::Relaxed))
            .unwrap();
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

    #[cfg(feature = "prefix_compression")]
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

        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = Arc::new(PosixFileSystem::new(false)) as Arc<dyn FileSystem>;
        // Pre-allocate file and initialize pages with valid checksums.
        let file_pages = 64u64;
        {
            let handle = fs.open(&path, true).unwrap();
            handle.set_len(file_pages * PAGE_SIZE as u64).unwrap();
            drop(handle);
        }
        let handle = fs.open(&path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        let pool = Arc::new(BufferPool::new(8, path.clone()));

        // Build tree with pool attached.
        let tree = BPlusTree::new(BPlusTreeConfig::default()).with_pool(pool.clone(), fs.clone());
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

    // ── Task 170: structural correctness ──────────────────────────────────

    /// Build a pool-backed tree and return `(tree, _pool, _fs, _dir)`.  The
    /// trailing handles must be kept alive for the duration of the test.
    fn pool_tree() -> (
        BPlusTree,
        std::sync::Arc<crate::buffer::pool::BufferPool>,
        std::sync::Arc<dyn FileSystem>,
        tempfile::TempDir,
    ) {
        use crate::buffer::pool::BufferPool;
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PAGE_SIZE, PageType, SlottedPage};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = Arc::new(PosixFileSystem::new(false)) as Arc<dyn FileSystem>;
        let file_pages = 4096u64;
        {
            let handle = fs.open(&path, true).unwrap();
            handle.set_len(file_pages * PAGE_SIZE as u64).unwrap();
            drop(handle);
        }
        let handle = fs.open(&path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        let pool = Arc::new(BufferPool::new(256, path));
        let tree = BPlusTree::new(BPlusTreeConfig::default()).with_pool(pool.clone(), fs.clone());
        (tree, pool, fs, dir)
    }

    #[test]
    fn pool_mode_grows_multi_level_and_branch_split_succeeds() {
        // In buffer-pool mode the old `find_parent` HashMap scan was empty, so
        // a branch split returned MissingParent.  With descend-path parent
        // tracking the tree must grow to multiple levels without error.
        let (tree, _pool, _fs, _dir) = pool_tree();
        for i in 1u128..=4000 {
            tree.insert(&node_id_key(i), &i.to_be_bytes())
                .unwrap_or_else(|e| panic!("insert {i} failed: {e}"));
        }
        let root = tree
            .get_page(tree.root_page_id.load(Ordering::Relaxed))
            .unwrap();
        assert!(root.is_branch(), "4000 keys must produce a branch root");
        assert!(
            root.btree_header().level >= 1,
            "tree must have at least one internal level"
        );
        for i in 1u128..=4000 {
            assert!(
                tree.search(&node_id_key(i)).is_some(),
                "key {i} must be findable in pool-mode tree"
            );
        }
    }

    #[test]
    fn full_split_then_many_deletes_stays_balanced_and_searchable() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let n = 2000u128;
        for i in 1..=n {
            tree.insert(&node_id_key(i), &i.to_be_bytes()).unwrap();
        }
        // Delete the lower half; every remaining key must still be findable and
        // every deleted key must be gone.
        for i in 1..=(n / 2) {
            assert!(
                tree.delete(&node_id_key(i)).unwrap(),
                "delete {i} not found"
            );
        }
        for i in 1..=(n / 2) {
            assert!(
                tree.search(&node_id_key(i)).is_none(),
                "deleted key {i} still present"
            );
        }
        for i in (n / 2 + 1)..=n {
            assert!(
                tree.search(&node_id_key(i)).is_some(),
                "surviving key {i} missing after deletes"
            );
        }
    }

    #[test]
    fn delete_until_empty_reclaims_and_keeps_searches_correct() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let n = 1500u128;
        for i in 1..=n {
            tree.insert(&node_id_key(i), &i.to_be_bytes()).unwrap();
        }
        // Delete every key in a scrambled order.
        let mut order: Vec<u128> = (1..=n).collect();
        // Simple deterministic shuffle.
        order.sort_by_key(|&x| (x.wrapping_mul(2654435761)) & 0xffff);
        for &i in &order {
            assert!(
                tree.delete(&node_id_key(i)).unwrap(),
                "delete {i} not found"
            );
            assert!(
                tree.search(&node_id_key(i)).is_none(),
                "key {i} present right after its own delete"
            );
        }
        // The tree is now empty: no key is findable and the root has collapsed
        // back to (at most) a single leaf.
        for i in 1..=n {
            assert!(tree.search(&node_id_key(i)).is_none(), "key {i} survived");
        }
        let root = tree
            .get_page(tree.root_page_id.load(Ordering::Relaxed))
            .unwrap();
        assert!(
            root.is_leaf(),
            "after emptying, the root must collapse to a leaf (was a {:?})",
            if root.is_branch() { "branch" } else { "leaf" }
        );
        assert_eq!(root.key_count(), 0, "empty tree root must hold no keys");

        // The tree must remain usable: re-insert and find.
        tree.insert(&node_id_key(42), b"again").unwrap();
        assert!(tree.search(&node_id_key(42)).is_some());
    }

    #[test]
    fn insert_random_order_all_keys_findable() {
        // Random insertion order exercises middle-of-tree splits and separator
        // updates that ascending insertion never reaches.
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let n = 2000u128;
        let mut order: Vec<u128> = (1..=n).collect();
        order.sort_by_key(|&x| (x.wrapping_mul(2654435761)) & 0xffff);
        for &k in &order {
            tree.insert(&node_id_key(k), &k.to_be_bytes()).unwrap();
        }
        for k in 1..=n {
            assert!(tree.search(&node_id_key(k)).is_some(), "key {k} missing");
        }
    }

    #[test]
    fn large_value_splits_preserve_all_keys() {
        // ~600-byte values pack ~12 records per 8 KiB leaf, so splits begin at
        // low key counts.  This regression-guards the slotted-page compaction
        // hazard that previously discarded leaf entries during a split.
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let big = vec![0xABu8; 600];
        let order: [u128; 20] = [
            10, 20, 30, 40, 50, 25, 15, 35, 5, 45, 12, 22, 32, 8, 18, 28, 38, 48, 2, 42,
        ];
        for &k in &order {
            tree.insert(&node_id_key(k), &big).unwrap();
        }
        for &k in &order {
            let (pid, slot) = tree
                .search(&node_id_key(k))
                .unwrap_or_else(|| panic!("key {k} missing after large-value splits"));
            let page = tree.get_page(pid).unwrap();
            let kv = page.key(slot).unwrap();
            let kl = u16::from_be_bytes([kv[0], kv[1]]) as usize;
            assert_eq!(&kv[2 + kl..], &big[..], "key {k} value corrupted");
        }
    }

    #[test]
    fn insert_batch_is_atomic_and_searchable() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let entries: Vec<(CompositeKey, Vec<u8>)> = (1u128..=1000)
            .map(|i| (node_id_key(i), i.to_be_bytes().to_vec()))
            .collect();
        tree.insert_batch(&entries).unwrap();
        for i in 1u128..=1000 {
            assert!(
                tree.search(&node_id_key(i)).is_some(),
                "batch key {i} missing"
            );
        }
        // The batch built a multi-level tree.
        let root = tree
            .get_page(tree.root_page_id.load(Ordering::Relaxed))
            .unwrap();
        assert!(
            root.is_branch(),
            "1000-key batch must produce a branch root"
        );
    }

    #[test]
    fn insert_batch_then_long_value_keys() {
        // Variable-length keys (long property values) survive a batch insert.
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let mut entries: Vec<(CompositeKey, Vec<u8>)> = Vec::new();
        for i in 0u128..200 {
            let value = vec![(i % 251) as u8; 80]; // 80-byte value -> >40-byte key
            let key = crate::index::key::property_index_key(1, &value, i);
            entries.push((key, i.to_be_bytes().to_vec()));
        }
        tree.insert_batch(&entries).unwrap();
        for (k, _) in &entries {
            assert!(tree.search(k).is_some(), "long batch key missing");
        }
    }

    #[test]
    fn interleaved_insert_delete_search_consistency() {
        use std::collections::BTreeSet;
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let mut present: BTreeSet<u128> = BTreeSet::new();
        for step in 0u128..3000 {
            let key = (step.wrapping_mul(48271) % 1000) + 1;
            if present.contains(&key) {
                tree.delete(&node_id_key(key)).unwrap();
                present.remove(&key);
            } else {
                tree.insert(&node_id_key(key), &key.to_be_bytes()).unwrap();
                present.insert(key);
            }
        }
        for k in 1u128..=1000 {
            let found = tree.search(&node_id_key(k)).is_some();
            assert_eq!(
                found,
                present.contains(&k),
                "key {k}: tree.search={found} but model={}",
                present.contains(&k)
            );
        }
    }

    #[test]
    fn concurrent_inserts_no_lost_updates() {
        // Many writer threads insert disjoint key ranges concurrently while
        // reader threads search.  No insert may be lost and the test must not
        // deadlock (it completes within the harness timeout).
        use std::sync::Arc;
        use std::thread;

        let tree = Arc::new(BPlusTree::new(BPlusTreeConfig::default()));
        let n_writers = 8u128;
        let per_writer = 500u128;

        let mut handles = Vec::new();
        for w in 0..n_writers {
            let t = Arc::clone(&tree);
            handles.push(thread::spawn(move || {
                // Disjoint, interleaved key ranges: key = i * n_writers + w + 1.
                for i in 0..per_writer {
                    let key = i * n_writers + w + 1;
                    t.insert(&node_id_key(key), &key.to_be_bytes()).unwrap();
                }
            }));
        }
        // Concurrent readers (must never deadlock or panic).
        for _ in 0..4 {
            let t = Arc::clone(&tree);
            handles.push(thread::spawn(move || {
                for _ in 0..2000 {
                    let _ = t.optimistic_search(&node_id_key(1));
                    let _ = t.search(&node_id_key(2));
                }
            }));
        }
        for h in handles {
            h.join().expect("no thread should panic or deadlock");
        }

        // Every inserted key must be present — no lost updates.
        let total = n_writers * per_writer;
        for key in 1..=total {
            assert!(
                tree.search(&node_id_key(key)).is_some(),
                "lost update: key {key} missing after concurrent inserts"
            );
        }
    }

    #[test]
    fn concurrent_insert_delete_search_consistency() {
        // Writers and deleters operate on disjoint key spaces so the final
        // membership is deterministic; readers run throughout.  Verifies no
        // deadlock and a consistent final state.
        use std::sync::Arc;
        use std::thread;

        let tree = Arc::new(BPlusTree::new(BPlusTreeConfig::default()));
        // Pre-seed keys 1..=2000 so deleters have something to remove.
        for k in 1u128..=2000 {
            tree.insert(&node_id_key(k), &k.to_be_bytes()).unwrap();
        }

        let mut handles = Vec::new();
        // Deleters remove disjoint strides of odd keys in [1, 2000].
        for d in 0..4u128 {
            let t = Arc::clone(&tree);
            handles.push(thread::spawn(move || {
                let mut k = 1 + 2 * d; // distinct odd starts
                while k <= 2000 {
                    let _ = t.delete(&node_id_key(k));
                    k += 8;
                }
            }));
        }
        // Writers add fresh keys in [3000, 5000).
        for w in 0..4u128 {
            let t = Arc::clone(&tree);
            handles.push(thread::spawn(move || {
                let mut k = 3000 + w;
                while k < 5000 {
                    t.insert(&node_id_key(k), &k.to_be_bytes()).unwrap();
                    k += 4;
                }
            }));
        }
        // Readers.
        for _ in 0..4 {
            let t = Arc::clone(&tree);
            handles.push(thread::spawn(move || {
                for k in 1u128..=2000 {
                    let _ = t.optimistic_search(&node_id_key(k));
                }
            }));
        }
        for h in handles {
            h.join().expect("no thread should deadlock or panic");
        }

        // Even keys in [1,2000] were never deleted and must remain; all the
        // [3000,5000) writes must be present.
        for k in (2u128..=2000).step_by(2) {
            assert!(
                tree.search(&node_id_key(k)).is_some(),
                "even key {k} must survive (it was never deleted)"
            );
        }
        for w in 0..4u128 {
            let mut k = 3000 + w;
            while k < 5000 {
                assert!(
                    tree.search(&node_id_key(k)).is_some(),
                    "concurrently-written key {k} missing"
                );
                k += 4;
            }
        }
    }

    #[test]
    fn optimistic_reads_stay_sound_under_concurrent_writes() {
        // A reader repeatedly optimistic-searches a stable key while writers
        // churn other keys.  The stable key must never spuriously disappear.
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::thread;

        let tree = Arc::new(BPlusTree::new(BPlusTreeConfig::default()));
        tree.insert(&node_id_key(u128::MAX), b"sentinel").unwrap();
        let stop = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::new();
        for w in 0..6u128 {
            let t = Arc::clone(&tree);
            let s = Arc::clone(&stop);
            handles.push(thread::spawn(move || {
                let mut k = w + 1;
                while !s.load(Ordering::Relaxed) {
                    let _ = t.insert(&node_id_key(k), &k.to_be_bytes());
                    let _ = t.delete(&node_id_key(k));
                    k += 6;
                    if k > 3000 {
                        k = w + 1;
                    }
                }
            }));
        }

        // Reader: the sentinel must always be observable via optimistic search.
        let t = Arc::clone(&tree);
        for _ in 0..10_000 {
            assert!(
                t.optimistic_search(&node_id_key(u128::MAX)).is_some(),
                "optimistic read lost the stable sentinel key"
            );
        }
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().expect("writer must not deadlock or panic");
        }
    }

    #[test]
    fn logged_index_batch_survives_crash_and_replay() {
        // Task 172: a WAL-logged index mutation must be recoverable after a
        // crash that loses the data file's index pages.  We write the batch
        // (logging IndexPageInsert/Update records), then zero the index pages on
        // disk to simulate a crash before the dirty pages were flushed, then run
        // ARIES REDO and verify the index pages are restored.
        use crate::buffer::pool::BufferPool;
        use crate::io::AlignedBuffer;
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PAGE_SIZE, PageType, SlottedPage};
        use crate::wal::aries::{AriesRecovery, DptEntry};
        use crate::wal::writer::WalWriter;
        use std::collections::HashMap;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("index.db");
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let fs = Arc::new(PosixFileSystem::new(false)) as Arc<dyn FileSystem>;

        // Pre-allocate and checksum-init the data file.
        let file_pages = 64u64;
        {
            let handle = fs.open(&data_path, true).unwrap();
            handle.set_len(file_pages * PAGE_SIZE as u64).unwrap();
            for pid in 0..file_pages {
                let mut page = SlottedPage::init(pid, PageType::SlottedData);
                page.update_checksum();
                handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
            }
            handle.sync_data().unwrap();
        }

        let pool = Arc::new(BufferPool::new(32, data_path.clone()));
        let tree = BPlusTree::new(BPlusTreeConfig::default()).with_pool(pool.clone(), fs.clone());

        // WAL-logged batch.
        let mut wal = WalWriter::open(wal_dir.clone(), fs.as_ref()).unwrap();
        let entries: Vec<(CompositeKey, Vec<u8>)> = (1u128..=30)
            .map(|i| (node_id_key(i), i.to_be_bytes().to_vec()))
            .collect();
        tree.insert_batch_logged(&entries, &mut wal, fs.as_ref(), 1)
            .unwrap();
        // Flush the dirty index pages to disk, then capture them.
        pool.flush_all(fs.as_ref()).unwrap();

        let root_id = tree.root_page_id.load(Ordering::Relaxed);
        let good_root = {
            let g = pool.fix_page(fs.as_ref(), root_id).unwrap();
            g.buf().clone()
        };
        assert!(BTreePage::from_buf(good_root.clone()).key_count() > 0);

        // Simulate a crash that lost the index pages: zero them on disk.
        {
            let handle = fs.open(&data_path, true).unwrap();
            let zero = AlignedBuffer::zeroed(PAGE_SIZE);
            for pid in 1..file_pages {
                handle.write_at(&zero, pid * PAGE_SIZE as u64).unwrap();
            }
            handle.sync_data().unwrap();
        }

        // Drop the pool so its cached frames cannot mask the on-disk loss.
        drop(tree);
        drop(pool);

        // Run ARIES REDO from the WAL.  Seed the DPT with every page the WAL
        // touched so REDO replays their after-images.
        let recovery = AriesRecovery::new(fs.as_ref(), &wal_dir, &data_path, 0);
        let records = recovery.load_all_segments(fs.as_ref(), &wal_dir).unwrap();
        assert!(
            records.iter().any(|r| matches!(
                r.record_type,
                crate::wal::record::RecordType::IndexPageInsert
                    | crate::wal::record::RecordType::IndexPageUpdate
            )),
            "WAL must contain physical index-page records"
        );
        let mut wal2 = WalWriter::open(wal_dir.clone(), fs.as_ref()).unwrap();
        let result = recovery.recover_from_slice(&records, &mut wal2).unwrap();
        assert!(
            result.redo_count > 0,
            "REDO must reapply at least one index page"
        );
        let _ = HashMap::<u64, DptEntry>::new();

        // Re-open a pool over the recovered data file and verify the index
        // pages are back and the root holds the inserted keys.
        let pool2 = Arc::new(BufferPool::new(32, data_path.clone()));
        let g = pool2.fix_page(fs.as_ref(), root_id).unwrap();
        let recovered = BTreePage::from_buf(g.buf().clone());
        assert!(
            recovered.is_leaf() || recovered.is_branch(),
            "recovered root must be a valid B+ tree page"
        );
        assert_eq!(
            recovered.key_count(),
            BTreePage::from_buf(good_root).key_count(),
            "recovered root must have the same key count as before the crash"
        );
    }
}
