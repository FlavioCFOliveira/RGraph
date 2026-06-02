//! Type secondary index.
//!
//! Maps `(type_id, edge_id)` -> [`SlotRef`] using a B+ tree.
//! Supports equality lookup and prefix range scans for all edges of a
//! given type.

use crate::graph::record::SlotRef;
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{type_index_key, CompositeKey};
use crate::index::page::BTreePage;

/// Secondary index for edge type lookups.
#[derive(Debug)]
pub struct TypeIndex {
    tree: BPlusTree,
}

impl TypeIndex {
    /// Create a new empty type index.
    pub fn new() -> Self {
        Self {
            tree: BPlusTree::new(BPlusTreeConfig::default()),
        }
    }

    /// Insert or update an entry.
    pub fn insert(
        &mut self,
        type_id: u64,
        edge_id: u128,
        slot: SlotRef,
    ) -> Result<(), BTreeError> {
        let key = type_index_key(type_id, edge_id);
        let value = slot.raw.to_be_bytes();
        self.tree.insert(&key, &value)
    }

    /// Delete an entry. Returns `true` if the key existed.
    pub fn delete(
        &mut self,
        type_id: u64,
        edge_id: u128,
    ) -> Result<bool, BTreeError> {
        let key = type_index_key(type_id, edge_id);
        self.tree.delete(&key)
    }

    /// Look up a single entry.
    pub fn lookup(&self,
        type_id: u64,
        edge_id: u128,
    ) -> Option<SlotRef> {
        let key = type_index_key(type_id, edge_id);
        let (_page_id, slot) = self.tree.search(&key)?;
        let page = self.tree.get_page(_page_id)?;
        decode_value(page, slot)
    }

    /// Scan all entries for `type_id` in `edge_id` order.
    pub fn scan(&self,
        type_id: u64,
    ) -> Vec<SlotRef> {
        let mut results = Vec::new();
        let start_key = type_index_key(type_id, 0);

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
            results.extend(scan_leaf(&leaf, type_id));
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

fn scan_leaf(page: &BTreePage, type_id: u64) -> Vec<SlotRef> {
    let mut results = Vec::new();
    let prefix = type_id.to_be_bytes();
    for i in 0..page.key_count() {
        if let Some(kv) = page.key(i) {
            let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
            let key = &kv[2..2 + key_len];
            // type_index_key = type_id (8 bytes) + edge_id (16 bytes)
            if key.len() >= 8 && key[..8] == prefix {
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
        let mut idx = TypeIndex::new();
        let slot = SlotRef::new(7, 2);
        idx.insert(10, 200, slot).unwrap();
        assert_eq!(idx.lookup(10, 200), Some(slot));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let idx = TypeIndex::new();
        assert!(idx.lookup(10, 999).is_none());
    }

    #[test]
    fn delete_removes_entry() {
        let mut idx = TypeIndex::new();
        let slot = SlotRef::new(7, 2);
        idx.insert(10, 200, slot).unwrap();
        assert!(idx.delete(10, 200).unwrap());
        assert!(idx.lookup(10, 200).is_none());
    }

    #[test]
    fn scan_returns_all_matching_type() {
        let mut idx = TypeIndex::new();
        for i in 1u128..=50 {
            idx.insert(5, i, SlotRef::new(i as u32, 0)).unwrap();
        }
        for i in 1u128..=10 {
            idx.insert(88, i, SlotRef::new(1000 + i as u32, 0)).unwrap();
        }

        let results = idx.scan(5);
        assert_eq!(results.len(), 50);
        for (i, slot) in results.iter().enumerate() {
            assert_eq!(slot.page_id(), (i + 1) as u32);
        }
    }

    #[test]
    fn scan_after_split() {
        let mut idx = TypeIndex::new();
        for i in 1u128..=500 {
            idx.insert(2, i, SlotRef::new(i as u32, 0)).unwrap();
        }
        let results = idx.scan(2);
        assert_eq!(results.len(), 500);
    }
}
