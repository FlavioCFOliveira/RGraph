//! Property secondary index.
//!
//! Maps `(property_id, serialized_value, entity_id)` -> [`SlotRef`] using a
//! B+ tree.  Supports equality lookup and prefix range scans for all
//! entities bearing a given property value.
//!
//! The serialized value is produced by [`encode_property_value`] so that
//! ordering is preserved across types.

use crate::graph::record::{SlotRef, ValueType};
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{property_index_key, CompositeKey};
use crate::index::page::BTreePage;
use crate::index::value_codec::encode_property_value;

/// Secondary index for property value lookups.
#[derive(Debug)]
pub struct PropertyIndex {
    tree: BPlusTree,
}

impl Default for PropertyIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl PropertyIndex {
    /// Create a new empty property index.
    pub fn new() -> Self {
        Self {
            tree: BPlusTree::new(BPlusTreeConfig::default()),
        }
    }

    /// Insert or update an entry.
    ///
    /// `entity_id` is the node or edge id that carries this property.
    pub fn insert(
        &mut self,
        property_id: u64,
        value_type: ValueType,
        payload: &[u8],
        entity_id: u128,
        slot: SlotRef,
    ) -> Result<(), BTreeError> {
        let serialized = encode_property_value(value_type, payload)
            .unwrap_or_else(|| vec![0x00]); // fallback to encoded NULL
        let key = property_index_key(property_id, &serialized, entity_id);
        let value = slot.raw.to_be_bytes();
        self.tree.insert(&key, &value)
    }

    /// Delete an entry. Returns `true` if the key existed.
    pub fn delete(
        &mut self,
        property_id: u64,
        value_type: ValueType,
        payload: &[u8],
        entity_id: u128,
    ) -> Result<bool, BTreeError> {
        let serialized = encode_property_value(value_type, payload)
            .unwrap_or_else(|| vec![0x00]);
        let key = property_index_key(property_id, &serialized, entity_id);
        self.tree.delete(&key)
    }

    /// Look up a single entry.
    pub fn lookup(
        &self,
        property_id: u64,
        value_type: ValueType,
        payload: &[u8],
        entity_id: u128,
    ) -> Option<SlotRef> {
        let serialized = encode_property_value(value_type, payload)
            .unwrap_or_else(|| vec![0x00]);
        let key = property_index_key(property_id, &serialized, entity_id);
        let (_page_id, slot) = self.tree.search(&key)?;
        let page = self.tree.get_page(_page_id)?;
        decode_value(page, slot)
    }

    /// Scan all entries for `(property_id, value)` in `entity_id` order.
    ///
    /// The scan follows leaf sibling pointers so it is correct across
    /// page boundaries and after splits.
    pub fn scan(
        &self,
        property_id: u64,
        value_type: ValueType,
        payload: &[u8],
    ) -> Vec<SlotRef> {
        let mut results = Vec::new();
        let serialized = encode_property_value(value_type, payload)
            .unwrap_or_else(|| vec![0x00]);
        // Prefix scan key: property_id + serialized_value + entity_id(0)
        let start_key = property_index_key(property_id, &serialized, 0);

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

        // Scan this leaf and all right siblings.
        loop {
            results.extend(scan_leaf(&leaf, property_id, &serialized,
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

/// Scan a single leaf page and return all entries whose property_id and
/// serialized value match.
fn scan_leaf(page: &BTreePage, property_id: u64, serialized_value: &[u8]) -> Vec<SlotRef> {
    let mut results = Vec::new();
    let prefix_len = 8 + serialized_value.len(); // property_id (8) + value bytes
    let expected_prefix = property_index_key(property_id, serialized_value, 0).as_slice()[..prefix_len]
        .to_vec();
    for i in 0..page.key_count() {
        if let Some(kv) = page.key(i) {
            let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
            let key = &kv[2..2 + key_len];
            if key.len() >= prefix_len && key[..prefix_len] == expected_prefix {
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
        let mut idx = PropertyIndex::new();
        let slot = SlotRef::new(5, 3);
        idx.insert(1, ValueType::Int64, &42i64.to_be_bytes(), 100, slot).unwrap();
        assert_eq!(idx.lookup(1, ValueType::Int64, &42i64.to_be_bytes(), 100), Some(slot));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let idx = PropertyIndex::new();
        assert!(idx.lookup(1, ValueType::Int64, &99i64.to_be_bytes(), 999).is_none());
    }

    #[test]
    fn delete_removes_entry() {
        let mut idx = PropertyIndex::new();
        let slot = SlotRef::new(5, 3);
        idx.insert(1, ValueType::Int64, &42i64.to_be_bytes(), 100, slot).unwrap();
        assert!(idx.delete(1, ValueType::Int64, &42i64.to_be_bytes(), 100).unwrap());
        assert!(idx.lookup(1, ValueType::Int64, &42i64.to_be_bytes(), 100).is_none());
    }

    #[test]
    fn scan_returns_all_matching_property() {
        let mut idx = PropertyIndex::new();
        for i in 1u128..=50 {
            let payload = (i as i64).to_be_bytes();
            idx.insert(7, ValueType::Int64, &payload, i, SlotRef::new(i as u32, 0)).unwrap();
        }
        // Insert some with a different value.
        for i in 1u128..=10 {
            let payload = (1000i64 + i as i64).to_be_bytes();
            idx.insert(7, ValueType::Int64, &payload, i, SlotRef::new(1000 + i as u32, 0)).unwrap();
        }

        let results = idx.scan(7, ValueType::Int64, &42i64.to_be_bytes());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page_id(), 42);
    }

    #[test]
    fn scan_after_split() {
        let mut idx = PropertyIndex::new();
        for i in 1u128..=500 {
            let payload = (42i64).to_be_bytes();
            idx.insert(3, ValueType::Int64, &payload, i, SlotRef::new(i as u32, 0)).unwrap();
        }
        let results = idx.scan(3, ValueType::Int64, &42i64.to_be_bytes());
        assert_eq!(results.len(), 500);
    }

    #[test]
    fn string_property_roundtrip() {
        let mut idx = PropertyIndex::new();
        let slot = SlotRef::new(7, 2);
        let payload = b"hello".to_vec();
        idx.insert(10, ValueType::String, &payload, 200, slot).unwrap();
        assert_eq!(
            idx.lookup(10, ValueType::String, &payload, 200),
            Some(slot)
        );
    }
}
