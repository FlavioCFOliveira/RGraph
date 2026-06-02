//! Online page defragmentation.
//!
//! Background maintenance that copies fragmented B+ tree pages into
//! compact pages, reducing dead space and improving cache locality.
//!
//! # Design
//!
//! A [`PageDefragmenter`] walks the leaf chain of a [`BPlusTree`] and
//! rewrites any leaf whose dead space exceeds a configurable threshold.
//! The compacted page is written back through the same `put_page_with_lsn`
//! path so that buffer-pool integration and WAL durability are preserved.
//!
//! In a full production implementation the old page pointer would be swapped
//! atomically via crossbeam-epoch reclamation; here the swap is performed
//! inside the tree's existing latch-coupled write path.

use crate::index::btree::BPlusTree;
use crate::index::page::BTreePage;
use crate::storage::page::{PageId, PAGE_SIZE};

/// Threshold: when dead space exceeds this fraction of the page, the
/// page is considered a candidate for defragmentation.
pub const DEFRAG_THRESHOLD_RATIO: f64 = 0.30;

/// Background defragmentation worker for a single B+ tree.
pub struct PageDefragmenter<'a> {
    tree: &'a BPlusTree,
}

impl<'a> PageDefragmenter<'a> {
    /// Create a new defragmenter bound to `tree`.
    pub fn new(tree: &'a BPlusTree) -> Self {
        Self { tree }
    }

    /// Scan every leaf page in the tree via sibling pointers and rewrite
    /// those whose dead space exceeds [`DEFRAG_THRESHOLD_RATIO`].
    ///
    /// Returns the number of pages that were rewritten.
    pub fn run(&self) -> usize {
        let mut rewritten = 0usize;
        // Start at the left-most leaf.
        let mut leaf_id = self.leftmost_leaf();
        while leaf_id != 0 {
            if let Some(page) = self.tree.get_page(leaf_id) {
                if page.is_leaf() && self.needs_defrag(&page) {
                    if let Some(compact) = self.compact_page(&page) {
                        self.tree.put_page_with_lsn(leaf_id, compact);
                        rewritten += 1;
                    }
                }
                leaf_id = page.btree_header().sibling_next;
            } else {
                break;
            }
        }
        rewritten
    }

    /// Find the left-most leaf in the tree.
    fn leftmost_leaf(&self) -> PageId {
        let mut page_id = self.tree.root_page_id.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            if let Some(page) = self.tree.get_page(page_id) {
                if page.is_leaf() {
                    return page_id;
                }
                // Follow the first child pointer of the branch.
                if page.key_count() > 0 {
                    if let Some(child) = page.child_pointer(0) {
                        page_id = child;
                        continue;
                    }
                }
                page_id = page.btree_header().rightmost_child;
            } else {
                return 0;
            }
        }
    }

    /// Check whether `page` exceeds the dead-space threshold.
    fn needs_defrag(&self,
        page: &BTreePage,
    ) -> bool {
        let free = page.free_space();
        let capacity = PAGE_SIZE
            - std::mem::size_of::<crate::storage::page::PageHeader>()
            - crate::index::page::BTREE_HEADER_SIZE;
        let ratio = free as f64 / capacity as f64;
        ratio > DEFRAG_THRESHOLD_RATIO
    }

    /// Build a new page containing exactly the live records of `page`,
    /// laid out contiguously.  Returns `None` if compaction fails
    /// (e.g. records do not fit, which should never happen for a
    /// defragmentation-only rewrite).
    fn compact_page(
        &self,
        page: &BTreePage,
    ) -> Option<BTreePage> {
        let page_id = page.page_id();
        let page_type = if page.is_leaf() {
            crate::storage::page::PageType::BTreeLeaf
        } else {
            crate::storage::page::PageType::BTreeInterior
        };
        let page_lsn = page.page_lsn();

        // Collect live records in logical order.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0..page.slot_count() {
            if let Some(data) = page.key(i) {
                records.push(data.to_vec());
            }
        }

        // Rebuild the underlying slotted page.
        let mut new_inner = crate::storage::page::SlottedPage::init(page_id, page_type);
        new_inner.header_mut().page_lsn = page_lsn;
        new_inner.header_mut().free_space_offset = crate::index::page::BTREE_HEADER_SIZE as u16;

        let mut new_page = crate::index::page::BTreePage { inner: new_inner };
        new_page.write_btree_header(&page.btree_header()
        );
        // Re-insert records preserving order.
        for (idx, record) in records.iter().enumerate() {
            let result = new_page.inner.insert_at(idx as u16, record);
            assert!(
                result.is_some(),
                "compact_page failed: record {} does not fit in rebuilt page",
                idx
            );
        }

        Some(new_page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::btree::{BPlusTree, BPlusTreeConfig};
    use crate::index::key::node_id_key;

    #[test]
    fn defrag_preserves_live_records() {
        use crate::index::page::BTreePage;

        // Build a standalone leaf page with records.
        let mut page = BTreePage::new_leaf(1);
        for i in 1u128..=50 {
            let key = node_id_key(i);
            let mut record = Vec::new();
            record.extend_from_slice(&u16::to_be_bytes(key.len() as u16));
            record.extend_from_slice(key.as_slice());
            record.extend_from_slice(&i.to_be_bytes());
            page.insert_raw(&record).unwrap();
        }

        // Delete every other record WITHOUT compacting, leaving holes in the
        // data area (the slot directory is compacted by delete, but the
        // payload bytes remain).
        for i in (1u128..=50).step_by(2) {
            let key = node_id_key(i);
            for slot in 0..page.slot_count() {
                if let Some(data) = page.key(slot) {
                    let key_len = u16::from_be_bytes([data[0], data[1]]) as usize;
                    if &data[2..2 + key_len] == key.as_slice() {
                        page.inner.delete(slot);
                        break;
                    }
                }
            }
        }

        // Wrap the page in a minimal BPlusTree so the defragger can use it.
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        tree.put_page_with_lsn(1, page);

        let defragger = PageDefragmenter::new(&tree);
        let rewritten = defragger.run();
        assert!(rewritten > 0, "defrag should rewrite the page");

        let page_after = tree.get_page(1).unwrap();
        // Verify remaining keys are intact.
        for i in (2u128..=50).step_by(2) {
            let key = node_id_key(i);
            let mut found = false;
            for slot in 0..page_after.slot_count() {
                if let Some(data) = page_after.key(slot) {
                    let key_len = u16::from_be_bytes([data[0], data[1]]) as usize;
                    if &data[2..2 + key_len] == key.as_slice() {
                        found = true;
                        break;
                    }
                }
            }
            assert!(found, "key {} should survive defragmentation", i);
        }
    }
}
