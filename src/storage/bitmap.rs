use crate::storage::page::{PageId, PageType, PAGE_SIZE};
use crate::storage::page::SlottedPage;

/// Number of pages tracked by one bitmap page.
/// Data area = PAGE_SIZE - header = 8192 - 64 = 8128 bytes.
/// 8128 bytes * 8 bits/byte = 65024 pages ≈ 508 MiB.
pub const PAGES_PER_BITMAP: usize = (PAGE_SIZE - SlottedPage::HEADER_SIZE) * 8;

/// In-memory view of a bitmap page backed by a [`SlottedPage`].
#[derive(Debug)]
pub struct BitmapPage {
    pub page: SlottedPage,
}

impl BitmapPage {
    pub fn new(page_id: PageId) -> Self {
        Self {
            page: SlottedPage::init(page_id, PageType::Bitmap),
        }
    }

    pub fn from_buf(buf: crate::io::AlignedBuffer) -> Self {
        Self {
            page: SlottedPage::new(buf),
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

    /// Scan for the first free page starting from `start_local` and
    /// return its global id, or `None`.
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

    pub fn base_page_id(&self) -> PageId {
        // For Sprint 1 we keep a single bitmap page (page 1) that tracks
        // pages starting from 0.  This will be generalised in later
        // sprints to support multiple bitmap pages.
        0
    }

    fn local_index(&self, global_id: PageId) -> usize {
        (global_id - self.base_page_id()) as usize
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
        let mut bmp = BitmapPage::new(1);
        let pid = 5;
        assert!(!bmp.is_set(pid));
        bmp.allocate(pid);
        assert!(bmp.is_set(pid));
        bmp.free(pid);
        assert!(!bmp.is_set(pid));
    }

    #[test]
    fn find_first_free() {
        let mut bmp = BitmapPage::new(2);
        bmp.allocate(3);
        bmp.allocate(5);
        assert_eq!(bmp.find_first_free(0), Some(0));
        assert_eq!(bmp.find_first_free(4), Some(4));
    }
}
