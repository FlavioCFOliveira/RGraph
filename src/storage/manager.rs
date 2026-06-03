use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::bitmap::{BitmapPage, FreeListCache, PAGES_PER_BITMAP};
use crate::storage::meta::{encode_superblock, Superblock};
use crate::storage::page::{PageId, PAGE_SIZE};
use std::io;
use std::path::PathBuf;

/// Simple page manager that owns the meta page, bitmap pages, and an
/// in-memory free-list cache.
pub struct PageManager {
    /// Path to the data file (all pages are stored here).
    pub data_path: PathBuf,
    /// In-memory copy of the superblock.
    pub superblock: Superblock,
    /// Bitmap page currently loaded in memory (page 1 for small DBs).
    pub bitmap: BitmapPage,
    /// Cached free pages ready to allocate.
    pub free_cache: FreeListCache,
    /// Long-lived handle to the data file.
    data_handle: Option<Box<dyn crate::io::FileHandle>>,
}

impl std::fmt::Debug for PageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageManager")
            .field("data_path", &self.data_path)
            .field("superblock", &self.superblock)
            .field("bitmap", &self.bitmap)
            .field("free_cache", &self.free_cache)
            .finish()
    }
}

impl PageManager {
    pub const DATA_FILE: &str = "rgraph.db";
    pub const CACHE_BATCH: usize = 64;

    /// Initialise a brand-new page manager and open the data file.
    /// The file is pre-extended to three pages and `sync_all` is issued.
    pub fn init(data_path: PathBuf, page_size: u32, fs: &dyn FileSystem) -> io::Result<Self> {
        let mut sb = Superblock::new(page_size);
        sb.total_page_count = 3; // page 0 = superblock primary, page 1 = superblock mirror, page 2 = bitmap
        sb.free_page_count = 0;
        sb.next_free_page_id = 3;
        sb.update_checksum();

        let mut bitmap = BitmapPage::new(2);
        bitmap.allocate(0); // superblock primary
        bitmap.allocate(1); // superblock mirror
        bitmap.allocate(2); // bitmap page
        bitmap.page.update_checksum();

        let handle = fs.open(&data_path, true)?;
        let required_len = (3 * PAGE_SIZE) as u64;
        let current_len = handle.len().unwrap_or(0);
        if current_len < required_len {
            handle.set_len(required_len)?;
            handle.sync_all()?;
        }

        Ok(Self {
            data_path,
            superblock: sb,
            bitmap,
            free_cache: FreeListCache::new(Self::CACHE_BATCH),
            data_handle: Some(handle),
        })
    }

    /// Open an existing page manager from disk.  The caller must have
    /// already read and validated the superblock.
    pub fn open(data_path: PathBuf, sb: Superblock, bitmap_buf: AlignedBuffer, fs: &dyn FileSystem) -> io::Result<Self> {
        let bitmap = BitmapPage::from_buf(bitmap_buf);
        let handle = fs.open(&data_path, false)?;
        let mut pm = Self {
            data_path,
            superblock: sb,
            bitmap,
            free_cache: FreeListCache::new(Self::CACHE_BATCH),
            data_handle: Some(handle),
        };
        pm.rebuild_cache();
        Ok(pm)
    }

    /// Allocate a new page id.  Prioritises the free list, then extends
    /// the file.
    pub fn allocate_page(&mut self) -> PageId {
        if let Some(pid) = self.free_cache.pop() {
            self.bitmap.allocate(pid);
            self.superblock.free_page_count -= 1;
            return pid;
        }
        let pid = self.superblock.next_free_page_id;
        self.superblock.next_free_page_id += 1;
        self.superblock.total_page_count += 1;
        self.bitmap.allocate(pid);
        pid
    }

    /// Return a page to the free list.
    pub fn free_page(&mut self, page_id: PageId) {
        self.bitmap.free(page_id);
        self.free_cache.push(page_id);
        self.superblock.free_page_count += 1;
    }

    /// Read a page from disk into `buf` and verify its checksum.
    pub fn read_page(&self, _fs: &dyn FileSystem, page_id: PageId, buf: &mut AlignedBuffer) -> io::Result<()> {
        let handle = self.data_handle.as_ref().expect("data file not open");
        let offset = page_id * PAGE_SIZE as u64;
        handle.read_at(buf, offset)?;
        if !crate::storage::page::SlottedPage::verify_checksum_bytes(buf) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("page {} checksum mismatch", page_id),
            ));
        }
        Ok(())
    }

    /// Write `buf` to disk at `page_id`, updating the checksum first.
    pub fn write_page(&self, _fs: &dyn FileSystem, page_id: PageId, buf: &mut AlignedBuffer) -> io::Result<()> {
        crate::storage::page::SlottedPage::update_checksum_bytes(buf);
        let handle = self.data_handle.as_ref().expect("data file not open");
        let offset = page_id * PAGE_SIZE as u64;
        // Ensure the file is large enough for this page.
        let required_len = offset + PAGE_SIZE as u64;
        let current_len = handle.len()?;
        if current_len < required_len {
            handle.set_len(required_len)?;
            handle.sync_all()?;
        }
        handle.write_at(buf, offset)?;
        handle.sync_data()
    }

    /// Persist the current superblock to disk (both copies).
    pub fn sync_superblock(&self, _fs: &dyn FileSystem) -> io::Result<()> {
        let mut sb = self.superblock;
        sb.generation += 1;
        sb.update_checksum();
        let mut aligned = AlignedBuffer::zeroed(PAGE_SIZE);
        let encoded = encode_superblock(&sb);
        aligned[..encoded.len()].copy_from_slice(&encoded);
        let handle = self.data_handle.as_ref().expect("data file not open");
        // Primary copy at offset 0.
        handle.write_at(&aligned, 0)?;
        // Mirror copy at offset PAGE_SIZE.
        handle.write_at(&aligned, PAGE_SIZE as u64)?;
        handle.sync_all()
    }

    /// Persist the bitmap page to disk.
    pub fn sync_bitmap(&mut self, _fs: &dyn FileSystem) -> io::Result<()> {
        let page_id = self.bitmap.page.header().page_id;
        let handle = self.data_handle.as_ref().expect("data file not open");
        let buf = &mut self.bitmap.page.buf;
        crate::storage::page::SlottedPage::update_checksum_bytes(buf);
        let offset = page_id * PAGE_SIZE as u64;
        let required_len = offset + PAGE_SIZE as u64;
        let current_len = handle.len()?;
        if current_len < required_len {
            handle.set_len(required_len)?;
            handle.sync_all()?;
        }
        handle.write_at(buf, offset)?;
        handle.sync_data()
    }

    /// Rebuild the free-list cache by scanning the bitmap.
    fn rebuild_cache(&mut self) {
        self.free_cache.pages.clear();
        let base = self.bitmap.base_page_id();
        for i in 0..PAGES_PER_BITMAP {
            if self.free_cache.pages.len() >= self.free_cache.batch_size {
                break;
            }
            let pid = base + i as PageId;
            if pid >= 2 && !self.bitmap.is_set(pid) {
                self.free_cache.pages.push(pid);
            }
        }
    }

    /// Return every allocated page id (including metadata pages).
    pub fn allocated_pages(&self) -> Vec<PageId> {
        self.bitmap.allocated_pages()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use crate::io::fault::{FaultConfig, FaultInjectFileSystem, FaultKind, FaultRule, OpMask};

    fn temp_fs() -> (tempfile::TempDir, PosixFileSystem, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PageManager::DATA_FILE);
        let fs = PosixFileSystem::new(false);
        (dir, fs, path)
    }

    #[test]
    fn init_and_allocate() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path, PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        assert_eq!(pid, 3);
        let pid2 = pm.allocate_page();
        assert_eq!(pid2, 4);
    }

    #[test]
    fn free_and_reuse() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path, PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        pm.free_page(pid);
        let pid2 = pm.allocate_page();
        assert_eq!(pid, pid2); // reused
    }

    #[test]
    fn write_and_read_roundtrip() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        let mut page = crate::storage::page::SlottedPage::init(pid, crate::storage::page::PageType::SlottedData);
        page.buf[crate::storage::page::SlottedPage::HEADER_SIZE] = 0xAB;
        page.update_checksum();
        pm.write_page(&fs, pid, &mut page.buf).unwrap();

        let mut read_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        pm.read_page(&fs, pid, &mut read_buf).unwrap();
        assert_eq!(read_buf[crate::storage::page::SlottedPage::HEADER_SIZE], 0xAB);
    }

    #[test]
    fn read_page_detects_corruption() {
        use crate::storage::page::{PageType, SlottedPage};
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        let mut page = SlottedPage::init(pid, PageType::SlottedData);
        page.update_checksum();
        pm.write_page(&fs, pid, &mut page.buf).unwrap();

        // Corrupt a byte in the data area on disk via a separate handle.
        let handle = fs.open(&path, false).unwrap();
        let corrupt_offset = pid * PAGE_SIZE as u64 + SlottedPage::HEADER_SIZE as u64 + 10;
        handle.write_at(&[0xFF], corrupt_offset).unwrap();
        handle.sync_data().unwrap();

        let mut read_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        let result = pm.read_page(&fs, pid, &mut read_buf);
        assert!(result.is_err(), "corrupted page should fail checksum verification");
    }

    #[test]
    fn sync_all_after_file_growth() {
        let (_dir, inner_fs, path) = temp_fs();
        let fs = FaultInjectFileSystem::new(Box::new(inner_fs));

        // Init with no faults.
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        // Configure every sync_all to fail.
        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask { sync_all: true, ..OpMask::default() },
                kind: FaultKind::Eio,
                every_n: None,
            }],
        });

        let pid = pm.allocate_page();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        let result = pm.write_page(&fs, pid, &mut buf);
        assert!(result.is_err(), "write_page must call sync_all after set_len when growing");
    }
}
