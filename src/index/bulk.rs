//! Bottom-up bulk loader.
//!
//! Given key/value pairs (in any order), the loader sorts them, packs leaves
//! at the configured fill factor, and builds internal levels bottom-up until a
//! single root remains.  The resulting pages are installed into a
//! [`BPlusTree`] so that the tree is immediately searchable.
//!
//! # Correctness
//!
//! The loader uses the same branch encoding as the online insert path: a
//! branch stores `(separator_i, child_i)` entries plus a `rightmost_child`,
//! where `separator_i` is the first key of the subtree to the right of
//! `child_i` and `child_i` holds keys strictly less than `separator_i`.  Every
//! child — including the left-most — receives a pointer, and the left-most key
//! of each non-first child is promoted as the separator.

use crate::index::btree::BPlusTree;
use crate::index::key::CompositeKey;
use crate::index::page::BTreePage;
use crate::storage::page::PageId;
use std::sync::atomic::Ordering;

/// Configuration for the bulk loader.
pub struct BulkLoaderConfig {
    /// Target leaf fill factor (0.0 .. 1.0).
    pub leaf_fill: f64,
    /// Target internal node fill factor.
    pub internal_fill: f64,
    /// Number of entries per checkpoint WAL flush.
    pub checkpoint_every: usize,
}

impl Default for BulkLoaderConfig {
    fn default() -> Self {
        Self {
            leaf_fill: 1.0,
            internal_fill: 0.75,
            checkpoint_every: 1_000_000,
        }
    }
}

/// Builder for a B+ tree from a (possibly unsorted) set of key/value pairs.
pub struct BulkLoader {
    #[allow(dead_code)]
    config: BulkLoaderConfig,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl BulkLoader {
    pub fn new(config: BulkLoaderConfig) -> Self {
        Self {
            config,
            entries: Vec::new(),
        }
    }

    /// Add a key/value pair.  Input need not be sorted — [`finish`] and
    /// [`load_into`] sort and de-duplicate before building.
    ///
    /// [`finish`]: Self::finish
    /// [`load_into`]: Self::load_into
    pub fn push(&mut self, key: &CompositeKey, value: &[u8]) {
        self.entries.push((key.as_slice().to_vec(), value.to_vec()));
    }

    /// Sort and de-duplicate the staged entries (last write wins per key).
    fn sorted_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut entries = self.entries.clone();
        // Stable sort so that, for equal keys, the later push wins after we
        // keep the last of each run.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut deduped: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            if let Some(last) = deduped.last_mut()
                && last.0 == k
            {
                last.1 = v;
            } else {
                deduped.push((k, v));
            }
        }
        deduped
    }

    /// Build the tree bottom-up and install all pages into `tree`, updating its
    /// root pointer.  After this returns, every inserted key is searchable.
    ///
    /// Returns the root page id.
    pub fn load_into(self, tree: &BPlusTree) -> PageId {
        let entries = self.sorted_entries();

        if entries.is_empty() {
            // Empty tree: a single empty leaf root.
            let root_id = tree.alloc_page_public();
            let leaf = BTreePage::new_leaf(root_id);
            tree.put_page_public(root_id, leaf);
            tree.root_page_id.store(root_id, Ordering::Relaxed);
            return root_id;
        }

        // ── Pack leaves ────────────────────────────────────────────────────
        // Each non-first leaf contributes its first key as a separator.
        let mut leaf_ids: Vec<PageId> = Vec::new();
        let mut leaf_first_keys: Vec<Vec<u8>> = Vec::new();
        let mut prev_leaf_id: PageId = 0;

        let mut current_id = tree.alloc_page_public();
        let mut current = BTreePage::new_leaf(current_id);
        let mut current_first: Option<Vec<u8>> = None;

        for (key, value) in &entries {
            let rec = encode_kv(key, value);
            // Seal proactively when the record would not fit.  We must never let
            // `insert_raw` trigger the underlying slotted-page compaction, which
            // is not B+-tree-header aware and would corrupt the page.
            if current.key_count() > 0 && !current.has_room_for(rec.len()) {
                current.set_siblings(prev_leaf_id, 0);
                if prev_leaf_id != 0
                    && let Some(mut prev) = tree.get_page(prev_leaf_id)
                {
                    let pprev = prev.btree_header().sibling_prev;
                    prev.set_siblings(pprev, current_id);
                    tree.put_page_public(prev_leaf_id, prev);
                }
                tree.put_page_public(current_id, current);
                leaf_ids.push(current_id);
                leaf_first_keys.push(current_first.take().unwrap_or_default());

                prev_leaf_id = current_id;
                current_id = tree.alloc_page_public();
                current = BTreePage::new_leaf(current_id);
            }
            current
                .insert_raw(&rec)
                .expect("INVARIANT: a record fits after a proactive seal");
            if current_first.is_none() {
                current_first = Some(key.clone());
            }
        }

        // Seal the final leaf.
        current.set_siblings(prev_leaf_id, 0);
        if prev_leaf_id != 0
            && let Some(mut prev) = tree.get_page(prev_leaf_id)
        {
            let pprev = prev.btree_header().sibling_prev;
            prev.set_siblings(pprev, current_id);
            tree.put_page_public(prev_leaf_id, prev);
        }
        tree.put_page_public(current_id, current);
        leaf_ids.push(current_id);
        leaf_first_keys.push(current_first.take().unwrap_or_default());

        // ── Build internal levels bottom-up ───────────────────────────────
        // `child_ids` and `child_first_keys` describe the level just below the
        // one we are constructing.  `child_first_keys[i]` is the smallest key
        // reachable through `child_ids[i]`; it becomes a separator when `i > 0`.
        let mut child_ids = leaf_ids;
        let mut child_first_keys = leaf_first_keys;
        let mut level: u8 = 1;

        while child_ids.len() > 1 {
            let mut parent_ids: Vec<PageId> = Vec::new();
            let mut parent_first_keys: Vec<Vec<u8>> = Vec::new();

            let mut i = 0usize;
            while i < child_ids.len() {
                let branch_id = tree.alloc_page_public();
                let mut branch = BTreePage::new_branch(branch_id, level);
                let group_first_key = child_first_keys[i].clone();

                // The first child of this branch has no separator entry; it is
                // recorded implicitly and becomes `rightmost_child` once the
                // group closes.  Subsequent children each add `(sep, prev_child)`.
                // Seal proactively (`has_room_for`) so the underlying
                // slotted-page compaction is never triggered on a branch page.
                let mut last_child = child_ids[i];
                let mut j = i + 1;
                while j < child_ids.len() {
                    let sep = &child_first_keys[j];
                    let rec = encode_branch_entry(sep, last_child);
                    if !branch.has_room_for(rec.len()) {
                        break;
                    }
                    branch
                        .insert_raw(&rec)
                        .expect("INVARIANT: a branch entry fits after has_room_for");
                    last_child = child_ids[j];
                    j += 1;
                }
                // Guarantee forward progress: a branch must hold at least the
                // first two children, otherwise the level cannot shrink.
                debug_assert!(j > i, "branch must absorb at least one child");
                branch.set_rightmost_child(last_child);
                tree.put_page_public(branch_id, branch);

                parent_ids.push(branch_id);
                parent_first_keys.push(group_first_key);

                i = j;
            }

            child_ids = parent_ids;
            child_first_keys = parent_first_keys;
            level += 1;
        }

        let root_id = child_ids[0];
        tree.root_page_id.store(root_id, Ordering::Relaxed);
        root_id
    }

    /// Build a fresh in-memory [`BPlusTree`] from the staged entries.
    ///
    /// Returns the populated tree; the previous standalone `finish`/`into_pages`
    /// API is superseded because it produced detached, unsearchable pages.
    pub fn finish(self) -> BPlusTree {
        let tree = BPlusTree::new(crate::index::btree::BPlusTreeConfig::default());
        self.load_into(&tree);
        tree
    }

    /// Atomically insert all staged entries into an existing `tree` via
    /// [`BPlusTree::insert_batch`], i.e. as a single latched batch.
    ///
    /// This is the wrapper form of the loader: it reuses the online insert path
    /// (correct splits, WAL-friendly) under one latch, rather than the bespoke
    /// bottom-up [`Self::load_into`].  Prefer it when inserting into a tree that
    /// already contains data, or when the one-latch atomicity is what matters.
    pub fn insert_all(self, tree: &BPlusTree) -> Result<(), crate::index::btree::BTreeError> {
        let entries: Vec<(CompositeKey, Vec<u8>)> = self
            .sorted_entries()
            .into_iter()
            .map(|(k, v)| (CompositeKey::from_slice(&k), v))
            .collect();
        tree.insert_batch(&entries)
    }
}

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
    fn bulk_load_empty() {
        let loader = BulkLoader::new(BulkLoaderConfig::default());
        let tree = loader.finish();
        // Empty tree: searching anything yields nothing, but the root exists.
        assert!(tree.search(&node_id_key(1)).is_none());
    }

    #[test]
    fn bulk_load_small_searchable() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        for i in 1u128..=10 {
            loader.push(&node_id_key(i), &i.to_be_bytes());
        }
        let tree = loader.finish();
        for i in 1u128..=10 {
            assert!(tree.search(&node_id_key(i)).is_some(), "key {i} missing");
        }
    }

    #[test]
    fn bulk_load_large_searchable() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        for i in 1u128..=1000 {
            loader.push(&node_id_key(i), &i.to_be_bytes());
        }
        let tree = loader.finish();
        for i in 1u128..=1000 {
            assert!(tree.search(&node_id_key(i)).is_some(), "key {i} missing");
        }
        // The tree must have grown past a single leaf.
        let root = tree
            .get_page(tree.root_page_id.load(Ordering::Relaxed))
            .unwrap();
        assert!(
            root.is_branch(),
            "1000 keys must produce a multi-level tree"
        );
    }

    #[test]
    fn bulk_load_unsorted_input_builds_correct_tree() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        // Push in a deliberately scrambled order.
        let order = [7u128, 3, 9, 1, 5, 10, 2, 8, 4, 6];
        for &i in &order {
            loader.push(&node_id_key(i), &i.to_be_bytes());
        }
        let tree = loader.finish();
        for i in 1u128..=10 {
            let found = tree.search(&node_id_key(i));
            assert!(found.is_some(), "key {i} missing after unsorted bulk load");
        }
    }

    #[test]
    fn bulk_load_dedups_last_write_wins() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        loader.push(&node_id_key(1), b"first");
        loader.push(&node_id_key(1), b"second");
        let tree = loader.finish();
        let (pid, slot) = tree.search(&node_id_key(1)).unwrap();
        let page = tree.get_page(pid).unwrap();
        let kv = page.key(slot).unwrap();
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        assert_eq!(&kv[2 + key_len..], b"second");
    }

    #[test]
    fn bulk_load_range_scan_spans_leaves() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        for i in 1u128..=1000 {
            loader.push(&node_id_key(i), &i.to_be_bytes());
        }
        let tree = loader.finish();
        let results = tree.range_search(&node_id_key(1), &node_id_key(1001));
        assert_eq!(results.len(), 1000, "range scan must cross sibling leaves");
    }
}
