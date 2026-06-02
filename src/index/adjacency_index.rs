//! Adjacency secondary index.
//!
//! Maps `(source_id, edge_type, target_id)` -> [`SlotRef`] using a B+ tree.
//! Supports partial key scans (source_id + type) for outgoing edge traversal
//! without walking linked lists.

use crate::graph::record::SlotRef;
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{edge_adjacency_key, CompositeKey};
use crate::index::page::BTreePage;

/// Secondary index for adjacency lookups.
#[derive(Debug)]
pub struct AdjacencyIndex {
    tree: BPlusTree,
}

impl Default for AdjacencyIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl AdjacencyIndex {
    /// Create a new empty adjacency index.
    pub fn new() -> Self {
        Self {
            tree: BPlusTree::new(BPlusTreeConfig::default()),
        }
    }

    /// Insert or update an entry.
    pub fn insert(
        &mut self,
        source_id: u128,
        type_id: u64,
        target_id: u128,
        slot: SlotRef,
    ) -> Result<(), BTreeError> {
        let key = edge_adjacency_key(source_id, type_id, target_id);
        let value = slot.raw.to_be_bytes();
        self.tree.insert(&key, &value)
    }

    /// Delete an entry. Returns `true` if the key existed.
    pub fn delete(
        &mut self,
        source_id: u128,
        type_id: u64,
        target_id: u128,
    ) -> Result<bool, BTreeError> {
        let key = edge_adjacency_key(source_id, type_id, target_id);
        self.tree.delete(&key)
    }

    /// Look up a single entry.
    pub fn lookup(
        &self,
        source_id: u128,
        type_id: u64,
        target_id: u128,
    ) -> Option<SlotRef> {
        let key = edge_adjacency_key(source_id, type_id, target_id);
        let (_page_id, slot) = self.tree.search(&key)?;
        let page = self.tree.get_page(_page_id)?;
        decode_value(page, slot)
    }

    /// Scan all outgoing edges for `(source_id, type_id)`.
    ///
    /// The scan follows leaf sibling pointers so it is correct across
    /// page boundaries and after splits.
    pub fn scan_outgoing(
        &self,
        source_id: u128,
        type_id: u64,
    ) -> Vec<SlotRef> {
        let mut results = Vec::new();
        let start_key = edge_adjacency_key(source_id, type_id, 0);

        if self.tree.get_page(self.tree.root_page_id.load(std::sync::atomic::Ordering::Relaxed)).is_none() {
            return results;
        }

        let leaf_id = find_leaf(
            &self.tree,
            self.tree.root_page_id.load(std::sync::atomic::Ordering::Relaxed),
            &start_key,
        );
        let mut leaf = match self.tree.get_page(leaf_id) {
            Some(p) => p,
            None => return results,
        };

        loop {
            results.extend(scan_leaf(&leaf, source_id, type_id,
            ));
            let next = leaf.btree_header().sibling_next;
            if next == 0 {
                break;
            }
            leaf = match self.tree.get_page(next) {
                Some(p) => p,
                None => break,
            };
        }

        results
    }

    /// Scan all incoming edges for `target_id`.
    ///
    /// Note: this requires a full tree scan because the key ordering is
    /// `(source, type, target)`.  For production workloads an additional
    /// reverse adjacency index (target + type + source) should be built.
    pub fn scan_incoming(
        &self,
        target_id: u128,
    ) -> Vec<SlotRef> {
        let mut results = Vec::new();
        // Full tree scan: check every leaf.
        let root_id = self.tree.root_page_id.load(std::sync::atomic::Ordering::Relaxed);
        if self.tree.get_page(root_id).is_none() {
            return results;
        }
        let leaf_id = find_leaf(&self.tree, root_id,
            &edge_adjacency_key(0, 0, 0),
        );
        let mut leaf = match self.tree.get_page(leaf_id) {
            Some(p) => p,
            None => return results,
        };

        loop {
            for i in 0..leaf.key_count() {
                if let Some(kv) = leaf.key(i) {
                    let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
                    let key = &kv[2..2 + key_len];
                    // edge_adjacency_key = source_id (16) + type_id (8) + target_id (16)
                    if key.len() >= 40 {
                        let t = crate::index::key::decode_u128_be(&key[24..40]
                        );
                        if t == target_id {
                            let value = &kv[2 + key_len..];
                            if value.len() >= 4 {
                                let raw = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
                                results.push(SlotRef { raw });
                            }
                        }
                    }
                }
            }
            let next = leaf.btree_header().sibling_next;
            if next == 0 {
                break;
            }
            leaf = match self.tree.get_page(next) {
                Some(p) => p,
                None => break,
            };
        }

        results
    }
}

/// Decode the value (SlotRef) from a leaf slot.
fn decode_value(page: BTreePage, slot: u16) -> Option<SlotRef> {
    let kv = page.key(slot)?;
    let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
    let value = &kv[2 + key_len..];
    if value.len() < 4 {
        return None;
    }
    let raw = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    Some(SlotRef { raw })
}

/// Find the leaf page that should contain `key`.
fn find_leaf(tree: &BPlusTree, mut page_id: u64, key: &CompositeKey) -> u64 {
    loop {
        let page = match tree.get_page(page_id) {
            Some(p) => p,
            None => return page_id,
        };
        if page.is_leaf() {
            return page_id;
        }
        page_id = branch_child(&page, key);
    }
}

/// Given a branch page and a key, return the child page id to follow.
fn branch_child(page: &BTreePage, key: &CompositeKey) -> u64 {
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

/// Scan a single leaf page and return all entries whose source_id and type_id match.
fn scan_leaf(page: &BTreePage, source_id: u128, type_id: u64) -> Vec<SlotRef> {
    let mut results = Vec::new();
    let prefix = edge_adjacency_key(source_id, type_id, 0).as_slice()[..24].to_vec();
    for i in 0..page.key_count() {
        if let Some(kv) = page.key(i) {
            let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
            let key = &kv[2..2 + key_len];
            // edge_adjacency_key = source_id (16 bytes) + type_id (8 bytes) + target_id (16 bytes)
            if key.len() >= 24 && key[..24] == prefix {
                let value = &kv[2 + key_len..];
                if value.len() >= 4 {
                    let raw = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
                    results.push(SlotRef { raw });
                }
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_lookup() {
        let mut idx = AdjacencyIndex::new();
        let slot = SlotRef::new(5, 3);
        idx.insert(1, 10, 100, slot).unwrap();
        assert_eq!(idx.lookup(1, 10, 100), Some(slot));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let idx = AdjacencyIndex::new();
        assert!(idx.lookup(1, 10, 999).is_none());
    }

    #[test]
    fn delete_removes_entry() {
        let mut idx = AdjacencyIndex::new();
        let slot = SlotRef::new(5, 3);
        idx.insert(1, 10, 100, slot).unwrap();
        assert!(idx.delete(1, 10, 100).unwrap());
        assert!(idx.lookup(1, 10, 100).is_none());
    }

    #[test]
    fn scan_outgoing_returns_all_matching() {
        let mut idx = AdjacencyIndex::new();
        for i in 1u128..=50 {
            idx.insert(7, 3, i, SlotRef::new(i as u32, 0)).unwrap();
        }
        // Insert some with a different source.
        for i in 1u128..=10 {
            idx.insert(99, 3, i, SlotRef::new(1000 + i as u32, 0)).unwrap();
        }

        let results = idx.scan_outgoing(7, 3);
        assert_eq!(results.len(), 50);
        for (i, slot) in results.iter().enumerate() {
            assert_eq!(slot.page_id(), (i + 1) as u32);
        }
    }

    #[test]
    fn scan_outgoing_after_split() {
        let mut idx = AdjacencyIndex::new();
        for i in 1u128..=500 {
            idx.insert(3, 2, i, SlotRef::new(i as u32, 0)).unwrap();
        }
        let results = idx.scan_outgoing(3, 2);
        assert_eq!(results.len(), 500);
    }

    #[test]
    fn scan_incoming_finds_target() {
        let mut idx = AdjacencyIndex::new();
        idx.insert(1, 10, 42, SlotRef::new(1, 0)).unwrap();
        idx.insert(2, 10, 42, SlotRef::new(2, 0)).unwrap();
        idx.insert(3, 10, 99, SlotRef::new(3, 0)).unwrap();

        let results = idx.scan_incoming(42);
        assert_eq!(results.len(), 2);
    }
}
