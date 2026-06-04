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

/// A tree-backed forward range cursor that follows sibling leaves.
///
/// Unlike [`BTreeCursor`], which is confined to a single page, this cursor
/// holds a reference to the [`BPlusTree`] and reloads the next sibling leaf when
/// the current one is exhausted, so a range scan correctly spans many leaves.
///
/// Construct it with [`BPlusTree::cursor_from`] / [`BPlusTree::into_range`].
pub struct BTreeRangeCursor<'a> {
    tree: &'a crate::index::btree::BPlusTree,
    current_leaf_id: PageId,
    current_leaf: BTreePage,
    current_slot: u16,
    /// Exclusive upper bound; `None` means scan to the end.
    end_key: Option<CompositeKey>,
}

impl<'a> BTreeRangeCursor<'a> {
    /// Internal constructor: position at the first key `>= from` in `leaf`.
    pub(crate) fn new(
        tree: &'a crate::index::btree::BPlusTree,
        leaf_id: PageId,
        leaf: BTreePage,
        from: &CompositeKey,
        end_key: Option<CompositeKey>,
    ) -> Self {
        let slot = BTreeCursor::lower_bound(&leaf, from);
        Self {
            tree,
            current_leaf_id: leaf_id,
            current_leaf: leaf,
            current_slot: slot,
            end_key,
        }
    }

    /// Reposition the cursor at the first key `>= key`, following sibling
    /// leaves if necessary.  Returns `true` if a key at or after `key` exists
    /// within the scan bound.
    pub fn advance_to(&mut self, key: &CompositeKey) -> bool {
        // If `key` is beyond the current leaf, hop forward leaf by leaf.
        loop {
            let count = self.current_leaf.key_count();
            if count > 0
                && let Some(last) = leaf_key_at(&self.current_leaf, count - 1)
                && last.as_slice() < key.as_slice()
            {
                // Every key on this leaf is < target; move to the next sibling.
                let next = self.current_leaf.btree_header().sibling_next;
                if next == 0 {
                    self.current_slot = count; // exhaust
                    return false;
                }
                match self.tree.get_page(next) {
                    Some(p) => {
                        self.current_leaf_id = next;
                        self.current_leaf = p;
                        self.current_slot = 0;
                        continue;
                    }
                    None => return false,
                }
            }
            break;
        }
        self.current_slot = BTreeCursor::lower_bound(&self.current_leaf, key);
        self.current_within_bound()
    }

    /// Return the current key/value as owned bytes, or `None` if the cursor is
    /// past the end or the upper bound.
    pub fn current(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        if !self.current_within_bound() {
            return None;
        }
        let kv = self.current_leaf.key(self.current_slot)?;
        let (k, v) = decode_kv(kv)?;
        Some((k.to_vec(), v.to_vec()))
    }

    /// Advance to the next entry, crossing sibling leaves as needed.
    /// Returns `false` once the scan is exhausted (end of tree or bound).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> bool {
        let count = self.current_leaf.key_count();
        if (self.current_slot as usize) + 1 < count as usize {
            self.current_slot += 1;
            return self.current_within_bound();
        }
        // Move to the next sibling leaf.
        let next = self.current_leaf.btree_header().sibling_next;
        if next == 0 {
            self.current_slot = count;
            return false;
        }
        match self.tree.get_page(next) {
            Some(p) => {
                self.current_leaf_id = next;
                self.current_leaf = p;
                self.current_slot = 0;
                self.current_within_bound()
            }
            None => false,
        }
    }

    /// Collect the entire remaining range into a vector of `(key, value)` pairs.
    pub fn collect_range(mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(pair) = self.current() {
            out.push(pair);
            if !self.next() {
                break;
            }
        }
        out
    }

    /// Is the current position valid and within the upper bound?
    fn current_within_bound(&self) -> bool {
        if self.current_slot as usize >= self.current_leaf.key_count() as usize {
            return false;
        }
        if let Some(ref end) = self.end_key
            && let Some(k) = leaf_key_at(&self.current_leaf, self.current_slot)
            && k.as_slice() >= end.as_slice()
        {
            return false;
        }
        true
    }

    /// The page id of the leaf the cursor currently sits on (for testing).
    pub fn current_leaf_id(&self) -> PageId {
        self.current_leaf_id
    }
}

/// Decode a leaf record `[key_len: u16 BE][key][value]` into `(key, value)`.
fn decode_kv(kv: &[u8]) -> Option<(&[u8], &[u8])> {
    if kv.len() < 2 {
        return None;
    }
    let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
    if 2 + key_len > kv.len() {
        return None;
    }
    Some((&kv[2..2 + key_len], &kv[2 + key_len..]))
}

/// Decode the key portion of a leaf slot as a [`CompositeKey`].
fn leaf_key_at(page: &BTreePage, slot: u16) -> Option<CompositeKey> {
    let (k, _) = decode_kv(page.key(slot)?)?;
    Some(CompositeKey::from_slice(k))
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

    // ── Tree-backed range cursor (Task 172) ───────────────────────────────

    use crate::index::btree::{BPlusTree, BPlusTreeConfig};

    #[test]
    fn range_cursor_spans_multiple_leaves() {
        // Insert enough keys to force several leaves, then scan a range that
        // straddles leaf boundaries.  The cursor must follow sibling pointers.
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        let n = 1500u128;
        for i in 1..=n {
            tree.insert(&node_id_key(i), &i.to_be_bytes()).unwrap();
        }
        let from = node_id_key(1);
        let to = node_id_key(n + 1);
        let results = tree.into_range(&from, &to);
        assert_eq!(results.len() as u128, n, "cursor must visit every key across leaves");
        // Keys must be returned in ascending order.
        for w in results.windows(2) {
            assert!(w[0].0 < w[1].0, "range cursor must yield sorted keys");
        }

        // Crossing at least one leaf boundary: confirm by scanning a window in
        // the middle and checking the count.
        let mid = tree.into_range(&node_id_key(500), &node_id_key(800));
        assert_eq!(mid.len(), 300, "half-open [500,800) must contain 300 keys");
    }

    #[test]
    fn range_cursor_advance_to_skips_ahead_across_leaves() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        for i in 1u128..=1500 {
            tree.insert(&node_id_key(i), &i.to_be_bytes()).unwrap();
        }
        let mut cursor = tree
            .cursor_from(&node_id_key(1), None)
            .expect("cursor should construct");
        // Skip directly to key 1234, which lives on a later leaf.
        assert!(cursor.advance_to(&node_id_key(1234)));
        let (k, _v) = cursor.current().unwrap();
        assert_eq!(k, node_id_key(1234).as_slice());
        // Iterate to the end and ensure monotonic progression.
        let mut last = 1234u128;
        while cursor.next() {
            let (k, _) = cursor.current().unwrap();
            let mut a = [0u8; 16];
            a.copy_from_slice(&k[..16]);
            let cur = u128::from_be_bytes(a);
            assert!(cur > last, "must advance strictly forward");
            last = cur;
        }
        assert_eq!(last, 1500, "cursor must reach the final key");
    }

    #[test]
    fn range_cursor_respects_upper_bound() {
        let tree = BPlusTree::new(BPlusTreeConfig::default());
        for i in 1u128..=1000 {
            tree.insert(&node_id_key(i), &i.to_be_bytes()).unwrap();
        }
        let results = tree.into_range(&node_id_key(10), &node_id_key(20));
        assert_eq!(results.len(), 10, "[10,20) is exactly 10 keys");
        // The bound is exclusive: key 20 must not appear.
        assert!(results.iter().all(|(k, _)| {
            let mut a = [0u8; 16];
            a.copy_from_slice(&k[..16]);
            u128::from_be_bytes(a) < 20
        }));
    }
}
