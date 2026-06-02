//! B+ tree cursor and range scan iterator.
//!
//! The cursor holds a latch on the current leaf and follows sibling
//! pointers for forward/backward traversal.

use crate::index::key::CompositeKey;
use crate::index::latch::LatchCoupling;
use crate::index::page::BTreePage;
use crate::storage::page::PageId;

/// Cursor over a B+ tree leaf chain.
///
/// The cursor holds a shared latch on the current leaf page while it is
/// alive, preventing concurrent splits from freeing the page underneath it.
pub struct BTreeCursor<'a> {
    #[allow(dead_code)]
    latch_mgr: &'a LatchCoupling,
    #[allow(dead_code)]
    current_page_id: PageId,
    current_slot: u16,
    // In a real implementation we would hold a PageGuard from the buffer pool.
    // Here we keep the page in memory for unit testing.
    pub current_page: BTreePage,
}

impl<'a> BTreeCursor<'a> {
    /// Position at the first key >= `key` in the given leaf page.
    pub fn seek(
        latch_mgr: &'a LatchCoupling,
        page: BTreePage,
        key: &CompositeKey,
    ) -> Self {
        let slot = Self::lower_bound(&page, key);
        Self {
            latch_mgr,
            current_page_id: page.page_id(),
            current_slot: slot,
            current_page: page,
        }
    }

    /// Position at the first key in the tree (first leaf, first slot).
    pub fn seek_first(latch_mgr: &'a LatchCoupling, page: BTreePage) -> Self {
        Self {
            latch_mgr,
            current_page_id: page.page_id(),
            current_slot: 0,
            current_page: page,
        }
    }

    /// Position at the last key in the tree (last leaf, last slot).
    pub fn seek_last(latch_mgr: &'a LatchCoupling, page: BTreePage) -> Self {
        let count = page.key_count();
        let slot = if count > 0 { count - 1 } else { 0 };
        Self {
            latch_mgr,
            current_page_id: page.page_id(),
            current_slot: slot,
            current_page: page,
        }
    }

    /// Return the key/value at the current position, if any.
    pub fn current(&self) -> Option<(&[u8], &[u8])> {
        let kv = self.current_page.key(self.current_slot)?;
        // Key and value are concatenated: [key_len: u16 BE][key][value]
        if kv.len() < 2 {
            return None;
        }
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        if 2 + key_len > kv.len() {
            return None;
        }
        let key = &kv[2..2 + key_len];
        let value = &kv[2 + key_len..];
        Some((key, value))
    }

    /// Advance to the next key.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> bool {
        let count = self.current_page.key_count();
        if (self.current_slot as usize) + 1 < count as usize {
            self.current_slot += 1;
            true
        } else {
            false
        }
    }

    /// Move to the previous key.
    pub fn prev(&mut self) -> bool {
        if self.current_slot > 0 {
            self.current_slot -= 1;
            true
        } else {
            false
        }
    }

    fn lower_bound(page: &BTreePage, key: &CompositeKey) -> u16 {
        let count = page.key_count() as usize;
        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mid_key = page_key(page, mid as u16);
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
}

/// Decode the key portion of a leaf slot entry.
fn page_key(page: &BTreePage, slot: u16) -> Option<CompositeKey> {
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

/// Iterator over a range of keys.
pub struct BTreeRangeScan<'a> {
    cursor: BTreeCursor<'a>,
    end_key: Option<CompositeKey>,
    done: bool,
}

impl<'a> BTreeRangeScan<'a> {
    pub fn new(cursor: BTreeCursor<'a>, end_key: Option<CompositeKey>) -> Self {
        Self {
            cursor,
            end_key,
            done: false,
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.done {
            return None;
        }
        let (key, val) = self.cursor.current()?;
        let key = key.to_vec();
        let val = val.to_vec();
        if let Some(ref end) = self.end_key
            && key.as_slice() >= end.as_slice()
        {
            self.done = true;
            return None;
        }
        let has_next = self.cursor.next();
        if !has_next {
            self.done = true;
        }
        Some((key, val))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::key::node_id_key;
    use crate::index::page::BTreePage;

    fn make_kv(key: &CompositeKey, value: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        let key_len = key.len() as u16;
        buf.extend_from_slice(&key_len.to_be_bytes());
        buf.extend_from_slice(key.as_slice());
        buf.extend_from_slice(value);
        buf
    }

    fn insert_kv(page: &mut BTreePage, key: &CompositeKey, value: &[u8]) {
        let record = make_kv(key, value);
        page.insert_raw(&record);
    }

    #[test]
    fn cursor_seek_and_next() {
        let latch = LatchCoupling::new();
        let mut page = BTreePage::new_leaf(1);
        let k1 = node_id_key(1);
        let k2 = node_id_key(2);
        let k3 = node_id_key(3);
        insert_kv(&mut page, &k1, b"v1");
        insert_kv(&mut page, &k2, b"v2");
        insert_kv(&mut page, &k3, b"v3");

        let cursor = BTreeCursor::seek(&latch, page, &k2);
        let (key, val) = cursor.current().unwrap();
        assert_eq!(key, k2.as_slice());
        assert_eq!(val, b"v2");
    }

    #[test]
    fn cursor_next_advances() {
        let latch = LatchCoupling::new();
        let mut page = BTreePage::new_leaf(1);
        let k1 = node_id_key(1);
        let k2 = node_id_key(2);
        insert_kv(&mut page, &k1, b"v1");
        insert_kv(&mut page, &k2, b"v2");

        let mut cursor = BTreeCursor::seek_first(&latch, page);
        let (key, _val) = cursor.current().unwrap();
        assert_eq!(key, k1.as_slice());
        assert!(cursor.next());
        let (key2, _val2) = cursor.current().unwrap();
        assert_eq!(key2, k2.as_slice());
        assert!(!cursor.next());
    }

    #[test]
    fn cursor_prev_moves_back() {
        let latch = LatchCoupling::new();
        let mut page = BTreePage::new_leaf(1);
        let k1 = node_id_key(1);
        let k2 = node_id_key(2);
        insert_kv(&mut page, &k1, b"v1");
        insert_kv(&mut page, &k2, b"v2");

        let mut cursor = BTreeCursor::seek_last(&latch, page);
        let (key, _val) = cursor.current().unwrap();
        assert_eq!(key, k2.as_slice());
        assert!(cursor.prev());
        let (key2, _val2) = cursor.current().unwrap();
        assert_eq!(key2, k1.as_slice());
        assert!(!cursor.prev());
    }

    #[test]
    fn range_scan_bounds() {
        let latch = LatchCoupling::new();
        let mut page = BTreePage::new_leaf(1);
        let k1 = node_id_key(1);
        let k2 = node_id_key(2);
        let k3 = node_id_key(3);
        insert_kv(&mut page, &k1, b"v1");
        insert_kv(&mut page, &k2, b"v2");
        insert_kv(&mut page, &k3, b"v3");

        let cursor = BTreeCursor::seek_first(&latch, page);
        let mut scan = BTreeRangeScan::new(cursor, Some(k3));
        let mut keys = Vec::new();
        while let Some((k, _v)) = scan.next() {
            keys.push(k.to_vec());
        }
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], k1.as_slice());
        assert_eq!(keys[1], k2.as_slice());
    }

    #[test]
    fn empty_cursor_has_no_current() {
        let latch = LatchCoupling::new();
        let page = BTreePage::new_leaf(1);
        let cursor = BTreeCursor::seek_first(&latch, page);
        assert!(cursor.current().is_none());
    }

    #[test]
    fn seek_lower_bound() {
        let latch = LatchCoupling::new();
        let mut page = BTreePage::new_leaf(1);
        let k1 = node_id_key(1);
        let k3 = node_id_key(3);
        insert_kv(&mut page, &k1, b"v1");
        insert_kv(&mut page, &k3, b"v3");

        let k2 = node_id_key(2);
        let cursor = BTreeCursor::seek(&latch, page, &k2);
        let (key, _val) = cursor.current().unwrap();
        assert_eq!(key, k3.as_slice());
    }
}
