//! B+ tree page formats.
//!
//! Extends the slotted page format with B+ tree specific headers.

use crate::io::AlignedBuffer;
use crate::storage::page::{PageHeader, PageId, PageType, SlottedPage, PAGE_SIZE};
use crate::index::prefix::{common_prefix, compress_record, decompress_record, extract_key};

/// Size of the B+ tree specific header extension (after the 64-byte page header).
pub const BTREE_HEADER_SIZE: usize = 32;

/// B+ tree page type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BTreePageType {
    /// Interior (branch) node storing separator keys and child pointers.
    Branch = 0x02,
    /// Leaf node storing key/value pairs with sibling pointers.
    Leaf = 0x03,
}

/// B+ tree specific header fields (stored at offset 64 in the page).
///
/// Fields are ordered by size (largest first) to minimise padding and
/// keep the total struct size at exactly 32 bytes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct BTreeHeader {
    /// Previous sibling page id (for leaf chaining), or 0.
    pub sibling_prev: PageId,
    /// Next sibling page id (for leaf chaining), or 0.
    pub sibling_next: PageId,
    /// Rightmost child pointer (for branch nodes), or 0.
    pub rightmost_child: PageId,
    /// Number of keys currently stored in this page.
    pub key_count: u16,
    /// Length of the common prefix shared by all keys in this page.
    pub common_prefix_len: u16,
    /// Tree level (0 = leaf, 1 = first branch, etc.).
    pub level: u8,
    /// Reserved padding to keep `BTreeHeader` at 32 bytes.
    pub _reserved: [u8; 3],
}

impl BTreeHeader {
    pub fn new(level: u8) -> Self {
        Self {
            sibling_prev: 0,
            sibling_next: 0,
            rightmost_child: 0,
            key_count: 0,
            common_prefix_len: 0,
            level,
            _reserved: [0; 3],
        }
    }
}

/// Typed wrapper around a slotted page for B+ tree operations.
#[derive(Debug, Clone)]
pub struct BTreePage {
    pub inner: SlottedPage,
}

impl BTreePage {
    pub fn new_leaf(page_id: PageId) -> Self {
        let inner = SlottedPage::init(page_id, PageType::BTreeLeaf);
        let mut page = Self { inner };
        page.write_btree_header(&BTreeHeader::new(0));
        // Reserve space for the B+ tree header in free_space_offset.
        page.inner.header_mut().free_space_offset = BTREE_HEADER_SIZE as u16;
        page
    }

    pub fn new_branch(page_id: PageId, level: u8) -> Self {
        let inner = SlottedPage::init(page_id, PageType::BTreeInterior);
        let mut page = Self { inner };
        page.write_btree_header(&BTreeHeader::new(level));
        // Reserve space for the B+ tree header in free_space_offset.
        page.inner.header_mut().free_space_offset = BTREE_HEADER_SIZE as u16;
        page
    }

    pub fn from_buf(buf: AlignedBuffer) -> Self {
        Self {
            inner: SlottedPage::new(buf),
        }
    }

    pub fn page_id(&self) -> PageId {
        self.inner.header().page_id
    }

    pub fn page_lsn(&self) -> u64 {
        self.inner.header().page_lsn
    }

    pub fn set_page_lsn(&mut self, lsn: u64) {
        self.inner.header_mut().page_lsn = lsn;
    }

    /// Read the B+ tree header from offset 64.
    pub fn btree_header(&self) -> BTreeHeader {
        let offset = size_of::<PageHeader>();
        let bytes = &self.inner.buf[offset..offset + size_of::<BTreeHeader>()];
        // SAFETY: bytes length matches struct size.
        unsafe {
            std::ptr::read_unaligned(bytes.as_ptr() as *const BTreeHeader)
        }
    }

    /// Write the B+ tree header at offset 64.
    pub fn write_btree_header(&mut self, header: &BTreeHeader) {
        let offset = size_of::<PageHeader>();
        let bytes = unsafe {
            std::slice::from_raw_parts(
                header as *const _ as *const u8,
                size_of::<BTreeHeader>(),
            )
        };
        self.inner.buf[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// Is this a leaf page?
    pub fn is_leaf(&self) -> bool {
        self.inner.header().page_type == PageType::BTreeLeaf as u8
    }

    /// Is this a branch page?
    pub fn is_branch(&self) -> bool {
        self.inner.header().page_type == PageType::BTreeInterior as u8
    }

    /// Number of entries (keys) in this page.
    pub fn key_count(&self) -> u16 {
        self.btree_header().key_count
    }

    /// Read the raw key at slot `idx`.
    pub fn key(&self, idx: u16) -> Option<&[u8]> {
        self.inner.read(idx)
    }

    /// Read the child page id associated with the key at slot `idx`.
    /// For branch nodes, each entry is: `[key bytes | child_page_id: u64 BE]`.
    pub fn child_pointer(&self, idx: u16) -> Option<PageId> {
        let data = self.inner.read(idx)?;
        let key_len = data.len().saturating_sub(8);
        if key_len > data.len() {
            return None;
        }
        let ptr_bytes = &data[key_len..];
        Some(u64::from_be_bytes([
            ptr_bytes[0], ptr_bytes[1], ptr_bytes[2], ptr_bytes[3],
            ptr_bytes[4], ptr_bytes[5], ptr_bytes[6], ptr_bytes[7],
        ]))
    }

    /// Read the separator key at slot `idx` (without the child pointer suffix).
    pub fn separator_key(&self, idx: u16) -> Option<&[u8]> {
        let data = self.inner.read(idx)?;
        let key_len = data.len().saturating_sub(8);
        Some(&data[..key_len])
    }

    /// Insert a raw key/value record into this page.
    /// Returns the slot index or `None` if it does not fit.
    pub fn insert_raw(&mut self, record: &[u8]) -> Option<u16> {
        let slot = self.inner.insert(record)?;
        let mut bh = self.btree_header();
        bh.key_count += 1;
        self.write_btree_header(&bh);
        Some(slot)
    }

    /// Insert a raw record at a specific logical slot index.
    /// Existing slots are shifted to the right.
    pub fn insert_raw_at(&mut self, idx: u16, record: &[u8]) -> Option<u16> {
        let slot = self.inner.insert_at(idx, record)?;
        let mut bh = self.btree_header();
        bh.key_count += 1;
        self.write_btree_header(&bh);
        Some(slot)
    }

    /// Update the sibling pointers.
    pub fn set_siblings(&mut self, prev: PageId, next: PageId) {
        let mut bh = self.btree_header();
        bh.sibling_prev = prev;
        bh.sibling_next = next;
        self.write_btree_header(&bh);
    }

    /// Set the rightmost child pointer (branch nodes only).
    pub fn set_rightmost_child(&mut self, child: PageId) {
        let mut bh = self.btree_header();
        bh.rightmost_child = child;
        self.write_btree_header(&bh);
    }

    /// Delete the entry at `idx` and update key_count.
    pub fn delete(&mut self, idx: u16) -> bool {
        if self.inner.delete(idx) {
            let mut bh = self.btree_header();
            if bh.key_count > 0 {
                bh.key_count -= 1;
            }
            self.write_btree_header(&bh);
            // Compact immediately so slot_count stays in sync with key_count
            // and ordering remains contiguous.
            self.compact();
            true
        } else {
            false
        }
    }

    /// Read the common prefix stored in this page.
    ///
    /// The prefix is located immediately after the B+ tree header at offset 96
    /// and is stored as `[len: u16 BE][bytes…]`.
    pub fn common_prefix(&self) -> Vec<u8> {
        let bh = self.btree_header();
        let len = bh.common_prefix_len as usize;
        if len == 0 {
            return Vec::new();
        }
        let offset = size_of::<PageHeader>() + size_of::<BTreeHeader>() + 2;
        if offset + len > self.inner.buf.len() {
            return Vec::new();
        }
        self.inner.buf[offset..offset + len].to_vec()
    }

    /// Write a new common prefix into the page and update the header.
    ///
    /// Existing slot data is **not** rewritten; callers must call
    /// [`recompute_prefix`] when they want to compress existing records.
    pub fn set_common_prefix(&mut self,
        prefix: &[u8],
    ) {
        let max_prefix = 512usize; // generous upper bound
        let prefix = &prefix[..prefix.len().min(max_prefix)];
        let len = prefix.len();
        let header_end = size_of::<PageHeader>() + size_of::<BTreeHeader>();
        let storage_start = header_end + 2;
        let storage_end = storage_start + len;

        // Ensure we do not overwrite existing slot data.
        let slot_size = size_of::<crate::storage::page::Slot>();
        let first_slot_offset = PAGE_SIZE - self.inner.header().slot_count as usize * slot_size;
        if storage_end > first_slot_offset {
            // Prefix is too large to fit; silently truncate.
            return;
        }

        // Write [len: u16 BE][prefix bytes].
        self.inner.buf[header_end..header_end + 2]
            .copy_from_slice(&(len as u16).to_be_bytes());
        self.inner.buf[storage_start..storage_end].copy_from_slice(prefix);

        // Update header.
        let mut bh = self.btree_header();
        bh.common_prefix_len = len as u16;
        self.write_btree_header(&bh);

        // Adjust free_space_offset so future inserts do not clobber the prefix.
        let new_free = storage_end as u16;
        let current_free = self.inner.header().free_space_offset;
        if new_free > current_free {
            self.inner.header_mut().free_space_offset = new_free;
        }
    }

    /// Recompute the common prefix from all current keys and rewrite every
    /// slot so that each stores only the suffix.
    ///
    /// This is typically called immediately after a page split, when the
    /// remaining keys are highly homogeneous.
    pub fn recompute_prefix(&mut self,
    ) {
        // Gather full keys (ignoring values) from every live slot.
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0..self.slot_count() {
            if let Some(data) = self.inner.read(i) {
                records.push(data.to_vec());
                if let Some(key) = extract_key(data) {
                    keys.push(key.to_vec());
                }
            }
        }
        if keys.is_empty() {
            self.set_common_prefix(&[]);
            return;
        }

        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let prefix = common_prefix(&key_refs);
        if prefix.is_empty() {
            self.set_common_prefix(&[]);
            return;
        }

        // Rewrite each record with the compressed key.
        let mut compressed: Vec<Vec<u8>> = Vec::new();
        for rec in &records {
            if let Some(c) = compress_record(rec, &prefix) {
                compressed.push(c);
            } else {
                // Fallback: keep original if compression fails.
                compressed.push(rec.clone());
            }
        }

        // Rebuild the page with the new prefix and compressed records.
        let bh = self.btree_header();
        let page_id = self.page_id();
        let page_type = if self.is_leaf() {
            PageType::BTreeLeaf
        } else {
            PageType::BTreeInterior
        };
        let page_lsn = self.page_lsn();

        let mut new_inner = SlottedPage::init(page_id, page_type);
        new_inner.header_mut().page_lsn = page_lsn;
        self.inner = new_inner;
        self.write_btree_header(&bh);
        self.set_common_prefix(&prefix);

        for rec in &compressed {
            let _ = self.inner.insert(rec);
        }
    }

    /// Read the full key at slot `idx`, reconstructing the prefix if present.
    pub fn key_full(&self, idx: u16) -> Option<Vec<u8>> {
        let data = self.inner.read(idx)?;
        let prefix = self.common_prefix();
        if prefix.is_empty() {
            return Some(data.to_vec());
        }
        decompress_record(data, &prefix)
    }

    /// Read the full separator key at slot `idx` (branch nodes).
    pub fn separator_key_full(&self, idx: u16) -> Option<Vec<u8>> {
        let data = self.inner.read(idx)?;
        let key_len = data.len().saturating_sub(8);
        let suffix = &data[..key_len];
        let prefix = self.common_prefix();
        if prefix.is_empty() {
            return Some(suffix.to_vec());
        }
        let mut full = Vec::with_capacity(prefix.len() + suffix.len());
        full.extend_from_slice(&prefix);
        full.extend_from_slice(suffix);
        Some(full)
    }

    /// Free space available in the page (accounting for both headers).
    pub fn free_space(&self) -> usize {
        let base_free = self.inner.header().free_space();
        // The btree header is already part of the page, so no extra cost.
        base_free
    }

    /// Recompute and store the page checksum.
    pub fn update_checksum(&mut self) {
        self.inner.update_checksum();
    }

    pub fn verify_checksum(&self) -> bool {
        self.inner.verify_checksum()
    }

    /// Compact the page, preserving the B+ tree header and record order.
    pub fn compact(&mut self) {
        let bh = self.btree_header();
        let page_id = self.page_id();
        let page_type = if self.is_leaf() {
            PageType::BTreeLeaf
        } else {
            PageType::BTreeInterior
        };
        let page_lsn = self.page_lsn();

        // Collect live records in logical order.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0..self.slot_count() {
            if let Some(data) = self.inner.read(i) {
                records.push(data.to_vec());
            }
        }

        // Rebuild the underlying slotted page.
        let mut new_inner = SlottedPage::init(page_id, page_type);
        new_inner.header_mut().page_lsn = page_lsn;
        new_inner.header_mut().free_space_offset = BTREE_HEADER_SIZE as u16;
        self.inner = new_inner;
        self.write_btree_header(&bh);

        // Restore key_count from the header (it was already decremented by delete).
        // Re-insert records preserving order.
        for (idx, record) in records.iter().enumerate() {
            let result = self.inner.insert_at(idx as u16, record);
            assert!(
                result.is_some(),
                "compact failed: record {} does not fit in rebuilt page",
                idx
            );
        }
    }

    /// Slot count from the underlying slotted page.
    pub fn slot_count(&self) -> u16 {
        self.inner.header().slot_count
    }
}

use std::mem::size_of;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_init() {
        let page = BTreePage::new_leaf(42);
        assert!(page.is_leaf());
        assert!(!page.is_branch());
        assert_eq!(page.page_id(), 42);
        assert_eq!(page.key_count(), 0);
    }

    #[test]
    fn insert_raw_at_after_delete() {
        let mut page = BTreePage::new_leaf(1);
        let key1 = b"aaa";
        page.insert_raw(key1).unwrap();
        assert_eq!(page.key_count(), 1);
        page.delete(0);
        assert_eq!(page.key_count(), 0);
        let key2 = b"bbb";
        page.insert_raw_at(0, key2).unwrap();
        assert_eq!(page.key_count(), 1);
        assert_eq!(page.key(0).unwrap(), key2);
    }

    #[test]
    fn branch_init() {
        let page = BTreePage::new_branch(7, 2);
        assert!(page.is_branch());
        assert!(!page.is_leaf());
        assert_eq!(page.btree_header().level, 2);
    }

    #[test]
    fn insert_and_read_key() {
        let mut page = BTreePage::new_leaf(1);
        let key = b"hello";
        let slot = page.insert_raw(key).unwrap();
        assert_eq!(page.key(slot).unwrap(), key);
        assert_eq!(page.key_count(), 1);
    }

    #[test]
    fn siblings_roundtrip() {
        let mut page = BTreePage::new_leaf(1);
        page.set_siblings(10, 20);
        let bh = page.btree_header();
        assert_eq!(bh.sibling_prev, 10);
        assert_eq!(bh.sibling_next, 20);
    }

    #[test]
    fn rightmost_child_roundtrip() {
        let mut page = BTreePage::new_branch(1, 1);
        page.set_rightmost_child(99);
        assert_eq!(page.btree_header().rightmost_child, 99);
    }

    #[test]
    fn branch_entry_encoding() {
        let mut page = BTreePage::new_branch(1, 1);
        let mut record = vec![b'k'; 16];
        record.extend_from_slice(&100u64.to_be_bytes());
        let slot = page.insert_raw(&record).unwrap();
        assert_eq!(page.child_pointer(slot).unwrap(), 100);
        assert_eq!(page.separator_key(slot).unwrap(), &record[..16]);
    }

    #[test]
    fn checksum_persists() {
        let mut page = BTreePage::new_leaf(1);
        page.update_checksum();
        assert!(page.verify_checksum());
    }

    #[test]
    fn delete_updates_key_count() {
        let mut page = BTreePage::new_leaf(1);
        let slot = page.insert_raw(b"x").unwrap();
        assert_eq!(page.key_count(), 1);
        assert!(page.delete(slot));
        assert_eq!(page.key_count(), 0);
    }
}
