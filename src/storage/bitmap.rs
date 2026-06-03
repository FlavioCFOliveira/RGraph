use crate::storage::page::{PageId, PageType, PAGE_SIZE};
use crate::storage::page::SlottedPage;

/// Number of pages tracked by one bitmap page.
/// Data area = PAGE_SIZE - header = 8192 - 64 = 8128 bytes.
/// 8128 bytes * 8 bits/byte = 65024 pages ≈ 508 MiB per bitmap slot.
pub const PAGES_PER_BITMAP: usize = (PAGE_SIZE - SlottedPage::HEADER_SIZE) * 8;

/// In-memory view of a bitmap page backed by a [`SlottedPage`].
///
/// Each `BitmapPage` tracks [`PAGES_PER_BITMAP`] logical page ids starting
/// at `base_page_id()`.  Multiple `BitmapPage` instances form a chain:
/// slot 0 covers ids `[0, PAGES_PER_BITMAP)`, slot 1 covers
/// `[PAGES_PER_BITMAP, 2 * PAGES_PER_BITMAP)`, and so on.
///
/// The `bitmap_slot` field (0-based) determines which range of page ids
/// this instance is responsible for.
#[derive(Debug)]
pub struct BitmapPage {
    pub page: SlottedPage,
    /// 0-based index in the bitmap chain.
    pub bitmap_slot: u64,
}

impl BitmapPage {
    /// Create a new, empty bitmap page.
    ///
    /// `file_page_id` is the physical page id of this bitmap on disk.
    /// `bitmap_slot` is the 0-based position in the bitmap chain.
    pub fn new(file_page_id: PageId, bitmap_slot: u64) -> Self {
        Self {
            page: SlottedPage::init(file_page_id, PageType::Bitmap),
            bitmap_slot,
        }
    }

    /// Reconstruct a bitmap page from a raw buffer read from disk.
    ///
    /// `bitmap_slot` is the 0-based position in the bitmap chain.
    pub fn from_buf(buf: crate::io::AlignedBuffer, bitmap_slot: u64) -> Self {
        Self {
            page: SlottedPage::new(buf),
            bitmap_slot,
        }
    }

    /// Is page `global_id` free (bit = 0) or allocated (bit = 1)?
    pub fn is_set(&self, global_id: PageId) -> bool {
        let local = self.local_index(global_id);
        let byte = local / 8;
        let bit = local % 8;
        let data = &self.page.buf[SlottedPage::HEADER_SIZE..];
        (data[byte] & (1 << bit)) != 0
    }

    /// Set the bit for `global_id` to 1 (allocated).
    pub fn allocate(&mut self, global_id: PageId) {
        let local = self.local_index(global_id);
        let byte = local / 8;
        let bit = local % 8;
        let data = &mut self.page.buf[SlottedPage::HEADER_SIZE..];
        data[byte] |= 1 << bit;
    }

    /// Clear the bit for `global_id` to 0 (free).
    pub fn free(&mut self, global_id: PageId) {
        let local = self.local_index(global_id);
        let byte = local / 8;
        let bit = local % 8;
        let data = &mut self.page.buf[SlottedPage::HEADER_SIZE..];
        data[byte] &= !(1 << bit);
    }

    /// Scan for the first free page starting from `start_local` (local index)
    /// and return its global id, or `None`.
    pub fn find_first_free(&self, start_local: usize) -> Option<PageId> {
        let data = &self.page.buf[SlottedPage::HEADER_SIZE..];
        let base = self.base_page_id();
        for i in start_local..PAGES_PER_BITMAP {
            let byte = i / 8;
            let bit = i % 8;
            if (data[byte] & (1 << bit)) == 0 {
                return Some(base + i as PageId);
            }
        }
        None
    }

    /// First global page id managed by this bitmap.
    pub fn base_page_id(&self) -> PageId {
        self.bitmap_slot * PAGES_PER_BITMAP as PageId
    }

    /// Return every allocated page id tracked by this bitmap.
    pub fn allocated_pages(&self) -> Vec<PageId> {
        let mut ids = Vec::new();
        let base = self.base_page_id();
        let data = &self.page.buf[SlottedPage::HEADER_SIZE..];
        for i in 0..PAGES_PER_BITMAP {
            let byte = i / 8;
            let bit = i % 8;
            if (data[byte] & (1 << bit)) != 0 {
                ids.push(base + i as PageId);
            }
        }
        ids
    }

    /// Convert a global page id to the local (0-based) index within this bitmap.
    ///
    /// # Panics (debug only)
    ///
    /// Panics in debug builds if `global_id` is outside the range managed by
    /// this bitmap, catching programming errors early.
    fn local_index(&self, global_id: PageId) -> usize {
        let base = self.base_page_id();
        debug_assert!(
            global_id >= base && (global_id - base) < PAGES_PER_BITMAP as PageId,
            "global_id {} is out of range for bitmap_slot {} (base={}, limit={})",
            global_id,
            self.bitmap_slot,
            base,
            base + PAGES_PER_BITMAP as PageId,
        );
        (global_id - base) as usize
    }
}

/// In-memory cache of free page ranges fetched from bitmap pages.
#[derive(Debug)]
pub struct FreeListCache {
    /// Pre-fetched free page ids ready to hand out.
    pub pages: Vec<PageId>,
    /// How many pages to fetch from bitmap in one batch.
    pub batch_size: usize,
}

impl FreeListCache {
    pub fn new(batch_size: usize) -> Self {
        Self {
            pages: Vec::with_capacity(batch_size),
            batch_size,
        }
    }

    pub fn pop(&mut self) -> Option<PageId> {
        self.pages.pop()
    }

    pub fn push(&mut self, page_id: PageId) {
        self.pages.push(page_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_and_free() {
        let mut bmp = BitmapPage::new(1, 0);
        let pid = 5;
        assert!(!bmp.is_set(pid));
        bmp.allocate(pid);
        assert!(bmp.is_set(pid));
        bmp.free(pid);
        assert!(!bmp.is_set(pid));
    }

    #[test]
    fn find_first_free() {
        let mut bmp = BitmapPage::new(2, 0);
        bmp.allocate(3);
        bmp.allocate(5);
        assert_eq!(bmp.find_first_free(0), Some(0));
        assert_eq!(bmp.find_first_free(4), Some(4));
    }

    #[test]
    fn base_page_id_slot_zero() {
        let bmp = BitmapPage::new(2, 0);
        assert_eq!(bmp.base_page_id(), 0);
    }

    #[test]
    fn base_page_id_slot_one() {
        let bmp = BitmapPage::new(3, 1);
        assert_eq!(bmp.base_page_id(), PAGES_PER_BITMAP as PageId);
    }

    #[test]
    fn base_page_id_slot_two() {
        let bmp = BitmapPage::new(4, 2);
        assert_eq!(bmp.base_page_id(), 2 * PAGES_PER_BITMAP as PageId);
    }

    #[test]
    fn allocate_in_second_slot() {
        let mut bmp = BitmapPage::new(3, 1);
        let global_id = PAGES_PER_BITMAP as PageId + 7;
        assert!(!bmp.is_set(global_id));
        bmp.allocate(global_id);
        assert!(bmp.is_set(global_id));
        bmp.free(global_id);
        assert!(!bmp.is_set(global_id));
    }
}
