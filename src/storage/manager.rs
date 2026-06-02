use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::bitmap::{BitmapPage, FreeListCache, PAGES_PER_BITMAP};
use crate::storage::meta::{encode_superblock, Superblock};
use crate::storage::page::{PageId, PAGE_SIZE};
use std::io;
use std::path::PathBuf;

/// Simple page manager that owns the meta page, bitmap pages, and an
/// in-memory free-list cache.
#[derive(Debug)]
pub struct PageManager {
    /// Path to the data file (all pages are stored here).
    pub data_path: PathBuf,
    /// In-memory copy of the superblock.
    pub superblock: Superblock,
    /// Bitmap page currently loaded in memory (page 1 for small DBs).
    pub bitmap: BitmapPage,
    /// Cached free pages ready to allocate.
    pub free_cache: FreeListCache,
}

impl PageManager {
    pub const DATA_FILE: &str = "rgraph.db";
    pub const CACHE_BATCH: usize = 64;

    /// Initialise a brand-new page manager.  The caller must write
    /// the superblock and bitmap to disk afterwards.
    pub fn init(data_path: PathBuf, page_size: u32) -> io::Result<Self> {
        let mut sb = Superblock::new(page_size);
        sb.total_page_count = 2; // page 0 = superblock, page 1 = bitmap
        sb.free_page_count = 0;
        sb.next_free_page_id = 2;
        sb.update_checksum();

        let mut bitmap = BitmapPage::new(1);
        bitmap.allocate(0); // superblock
        bitmap.allocate(1); // bitmap page
        bitmap.page.update_checksum();

        Ok(Self {
            data_path,
            superblock: sb,
            bitmap,
            free_cache: FreeListCache::new(Self::CACHE_BATCH),
        })
    }

    /// Open an existing page manager from disk.  The caller must have
    /// already read and validated the superblock.
    pub fn open(data_path: PathBuf, sb: Superblock, bitmap_buf: AlignedBuffer) -> io::Result<Self> {
        let bitmap = BitmapPage::from_buf(bitmap_buf);
        let mut pm = Self {
            data_path,
            superblock: sb,
            bitmap,
            free_cache: FreeListCache::new(Self::CACHE_BATCH),
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

    /// Read a page from disk into `buf`.
    pub fn read_page(&self, fs: &dyn FileSystem, page_id: PageId, buf: &mut AlignedBuffer) -> io::Result<()> {
        let offset = page_id * PAGE_SIZE as u64;
        let handle = fs.open(&self.data_path, false)?;
        handle.read_at(buf, offset)
    }

    /// Write `buf` to disk at `page_id`.
    pub fn write_page(&self, fs: &dyn FileSystem, page_id: PageId, buf: &AlignedBuffer) -> io::Result<()> {
        let offset = page_id * PAGE_SIZE as u64;
        let handle = fs.open(&self.data_path, false)?;
        // Ensure the file is large enough for this page.
        let required_len = offset + PAGE_SIZE as u64;
        let current_len = handle.len()?;
        if current_len < required_len {
            handle.set_len(required_len)?;
        }
        handle.write_at(buf, offset)?;
        handle.sync_data()
    }

    /// Persist the current superblock to disk (both copies).
    pub fn sync_superblock(&self, fs: &dyn FileSystem) -> io::Result<()> {
        let mut sb = self.superblock;
        sb.generation += 1;
        sb.update_checksum();
        let buf = encode_superblock(&sb);
        let handle = fs.open(&self.data_path, false)?;
        // Primary copy at offset 0.
        handle.write_at(&buf, 0)?;
        // Mirror copy at offset PAGE_SIZE.
        handle.write_at(&buf, PAGE_SIZE as u64)?;
        handle.sync_data()
    }

    /// Persist the bitmap page to disk.
    pub fn sync_bitmap(&self, fs: &dyn FileSystem) -> io::Result<()> {
        let page_id = self.bitmap.page.header().page_id;
        self.write_page(fs, page_id, &self.bitmap.page.buf)
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
    use std::io::Write;

    fn temp_fs() -> (tempfile::TempDir, PosixFileSystem, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PageManager::DATA_FILE);
        let fs = PosixFileSystem::new(false);
        // Pre-create an empty file so open works.
        let mut f = std::fs::File::create(&path).unwrap();
        f.flush().unwrap();
        (dir, fs, path)
    }

    #[test]
    fn init_and_allocate() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path, PAGE_SIZE as u32).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        assert_eq!(pid, 2);
        let pid2 = pm.allocate_page();
        assert_eq!(pid2, 3);
    }

    #[test]
    fn free_and_reuse() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path, PAGE_SIZE as u32).unwrap();
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
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        let mut write_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        write_buf[0] = 0xAB;
        pm.write_page(&fs, pid, &write_buf).unwrap();

        let mut read_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        pm.read_page(&fs, pid, &mut read_buf).unwrap();
        assert_eq!(read_buf[0], 0xAB);
    }
}
