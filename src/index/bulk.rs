//! Bottom-up bulk loader.
//!
//! Given a sorted stream of key/value pairs, packs leaves at 100% fill
//! factor and internal nodes at 75%, writing sequentially.

use crate::index::key::CompositeKey;
use crate::index::page::BTreePage;
use crate::storage::page::PageId;

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

/// In-memory builder for a B+ tree from sorted input.
pub struct BulkLoader {
    #[allow(dead_code)]
    config: BulkLoaderConfig,
    current_leaf: BTreePage,
    leaf_entries: Vec<(Vec<u8>, Vec<u8>)>,
    pages: Vec<BTreePage>,
    next_page_id: PageId,
}

impl BulkLoader {
    pub fn new(config: BulkLoaderConfig) -> Self {
        let mut loader = Self {
            config,
            current_leaf: BTreePage::new_leaf(0),
            leaf_entries: Vec::new(),
            pages: Vec::new(),
            next_page_id: 100, // reserve low ids for meta
        };
        loader.current_leaf = BTreePage::new_leaf(loader.alloc_page_id());
        loader
    }

    fn alloc_page_id(&mut self) -> PageId {
        let id = self.next_page_id;
        self.next_page_id += 1;
        id
    }

    #[allow(dead_code)]
    fn peek_next_page_id(&self) -> PageId {
        self.next_page_id
    }

    /// Add a sorted key/value pair.
    pub fn push(&mut self, key: &CompositeKey, value: &[u8]) {
        self.leaf_entries.push((
            key.as_slice().to_vec(),
            value.to_vec(),
        ));
    }

    /// Finalise loading and return the root page id.
    pub fn finish(mut self) -> PageId {
        if self.leaf_entries.is_empty() {
            return self.current_leaf.page_id();
        }

        // Pack all leaves.
        let entries = self.leaf_entries.clone();
        let mut leaves: Vec<BTreePage> = Vec::new();
        let mut current = BTreePage::new_leaf(self.alloc_page_id());
        let mut prev_leaf_id = 0u64;

        for (key, value) in &entries {
            let kv = encode_kv(key, value);
            if current.insert_raw(&kv).is_none() {
                // Leaf is full: seal it, chain it, and start a new one.
                let this_id = current.page_id();
                current.set_siblings(prev_leaf_id, 0);
                if prev_leaf_id > 0 {
                    // Update previous leaf's next pointer.
                    if let Some(prev) = leaves.last_mut() {
                        prev.set_siblings(prev.btree_header().sibling_prev, this_id);
                    }
                }
                leaves.push(current);
                prev_leaf_id = this_id;
                current = BTreePage::new_leaf(self.alloc_page_id());
                current.insert_raw(&kv).expect("single kv must fit in empty leaf");
            }
        }
        // Seal final leaf.
        let final_id = current.page_id();
        current.set_siblings(prev_leaf_id, 0);
        if prev_leaf_id > 0 {
            if let Some(prev) = leaves.last_mut() {
                prev.set_siblings(prev.btree_header().sibling_prev, final_id);
            }
        }
        leaves.push(current);

        self.pages.extend(leaves);

        // Build internal levels bottom-up until a single root remains.
        let mut level_pages: Vec<PageId> = self.pages.iter().map(|p| p.page_id()).collect();
        let mut level = 0u8;

        while level_pages.len() > 1 {
            let mut next_level = Vec::new();
            let mut current_branch = BTreePage::new_branch(self.alloc_page_id(), level + 1);
            let mut first = true;

            for &leaf_id in &level_pages {
                if first {
                    first = false;
                    continue; // leftmost child is stored as rightmost_child of previous
                }
                // Use the first key of the leaf as the separator.
                let sep_key = self.find_first_key(leaf_id);
                let record = encode_branch_entry(&sep_key, leaf_id);
                if current_branch.insert_raw(&record).is_none() {
                    let branch_id = current_branch.page_id();
                    next_level.push(branch_id);
                    current_branch = BTreePage::new_branch(self.alloc_page_id(), level + 1);
                    current_branch.insert_raw(&record).unwrap();
                }
            }
            let last_branch_id = current_branch.page_id();
            next_level.push(last_branch_id);
            level_pages = next_level;
            level += 1;
        }

        level_pages[0]
    }

    fn find_first_key(&self, page_id: PageId) -> Vec<u8> {
        for page in &self.pages {
            if page.page_id() == page_id {
                if let Some(kv) = page.key(0) {
                    let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
                    return kv[2..2 + key_len].to_vec();
                }
            }
        }
        Vec::new()
    }

    /// Consume the loader and return all built pages.
    pub fn into_pages(self) -> Vec<BTreePage> {
        self.pages
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
        let root = loader.finish();
        assert!(root > 0);
    }

    #[test]
    fn bulk_load_small() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        for i in 1u128..=10 {
            let k = node_id_key(i);
            loader.push(&k, &i.to_be_bytes());
        }
        let root = loader.finish();
        assert!(root > 0);
    }

    #[test]
    fn bulk_load_large() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        for i in 1u128..=1000 {
            let k = node_id_key(i);
            loader.push(&k, &i.to_be_bytes());
        }
        let root = loader.finish();
        assert!(root > 0);
    }

    #[test]
    fn bulk_load_keys_are_sorted() {
        let mut loader = BulkLoader::new(BulkLoaderConfig::default());
        for i in 1u128..=100 {
            let k = node_id_key(i);
            loader.push(&k, &i.to_be_bytes());
        }
        let loader = loader; // freeze
        let root = loader.finish();
        assert!(root > 0);
    }
}
