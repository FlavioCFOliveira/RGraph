//! Double-write buffer for torn-page protection (Task 109).
//!
//! Before a batch of dirty pages is written to their final on-disk locations,
//! this module copies them sequentially to a dedicated doublewrite file.  Only
//! after that write has been synced are the pages flushed to their actual page
//! slots.
//!
//! On crash recovery, [`DoubleWriteBuffer::recover_torn_pages`] reads the
//! doublewrite file and, for each entry, computes the CRC32C of the page at
//! its final location.  If the checksum does not match (indicating a torn
//! write), the doublewrite copy is restored, making the recovery process
//! transparent to the WAL-based REDO phase.
//!
//! # File layout
//!
//! ```text
//! [4 bytes] count      — number of page entries that follow
//! [N entries]          — each entry:
//!     [8 bytes] page_id
//!     [PAGE_SIZE bytes] page image
//! ```
//!
//! # CRC verification
//!
//! The page header stores a CRC32C covering the entire page (header fields
//! with the `checksum` field zeroed, plus payload).  The [`SlottedPage`]
//! `compute_checksum` / `verify_checksum` methods implement this contract.
//! If a page's stored checksum does not match its computed value, the page is
//! considered torn and is restored from the doublewrite copy.

use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::page::PAGE_SIZE;
use std::io;
use std::path::{Path, PathBuf};

/// Maximum number of pages in one doublewrite batch.
pub const DOUBLEWRITE_CAPACITY: usize = 128;

/// Fixed byte length of one page entry in the doublewrite file.
const ENTRY_SIZE: usize = 8 + PAGE_SIZE; // page_id (8) + image (PAGE_SIZE)

/// Double-write buffer protecting against torn writes.
///
/// Create with [`DoubleWriteBuffer::open`].  The file is created if it does
/// not exist.
#[derive(Debug)]
pub struct DoubleWriteBuffer {
    /// Path of the doublewrite file.
    path: PathBuf,
    /// Maximum pages per write batch.
    capacity: usize,
}

impl DoubleWriteBuffer {
    /// Open (or create) the doublewrite file at `path`.
    pub fn open(path: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        // Ensure the parent directory exists.
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent)?;
        }
        // Touch the file if it does not yet exist.
        if !fs.exists(&path) {
            let handle = fs.open(&path, true)?;
            handle.sync_data()?;
        }
        Ok(Self {
            path,
            capacity: DOUBLEWRITE_CAPACITY,
        })
    }

    /// Write `pages` to the doublewrite file before the caller performs the
    /// final write to the database.
    ///
    /// The batch is limited to [`self.capacity`] pages; any pages beyond that
    /// limit are silently dropped from this batch (the caller is responsible
    /// for splitting large batches).
    ///
    /// # Format
    ///
    /// ```text
    /// [4 bytes big-endian count]
    /// per entry: [8 bytes page_id][PAGE_SIZE bytes image]
    /// ```
    pub fn write_batch(
        &self,
        pages: &[(u64, &[u8])],
        fs: &dyn FileSystem,
    ) -> io::Result<()> {
        let batch = &pages[..pages.len().min(self.capacity)];
        let count = batch.len() as u32;

        let total = 4 + batch.len() * ENTRY_SIZE;
        let mut buf = vec![0u8; total];

        buf[0..4].copy_from_slice(&count.to_be_bytes());
        let mut off = 4usize;
        for (page_id, image) in batch {
            buf[off..off + 8].copy_from_slice(&page_id.to_be_bytes());
            off += 8;
            let copy_len = image.len().min(PAGE_SIZE);
            buf[off..off + copy_len].copy_from_slice(&image[..copy_len]);
            off += PAGE_SIZE;
        }

        let handle = fs.open(&self.path, true)?;
        handle.write_at(&buf, 0)?;
        handle.set_len(total as u64)?;
        handle.sync_data()?;
        Ok(())
    }

    /// Recover any torn pages found in the final data file.
    ///
    /// For each page recorded in the doublewrite file, reads the corresponding
    /// page from `data_path` and verifies its CRC32C checksum.  If the
    /// checksum is wrong (torn write), the doublewrite copy is restored.
    ///
    /// Returns the number of pages that were restored.
    pub fn recover_torn_pages(
        &self,
        data_path: &Path,
        fs: &dyn FileSystem,
    ) -> io::Result<usize> {
        let entries = self.read_entries(fs)?;
        if entries.is_empty() {
            return Ok(0);
        }

        let mut restored = 0usize;

        for (page_id, dw_image) in &entries {
            let offset = page_id * PAGE_SIZE as u64;

            // Try to read the page from the final location.
            let is_torn = if !fs.exists(data_path) {
                true
            } else {
                let handle = fs.open(data_path, false)?;
                let file_len = handle.len()?;
                if file_len < offset + PAGE_SIZE as u64 {
                    // Page not yet present — treat as torn.
                    true
                } else {
                    let mut page_buf = AlignedBuffer::zeroed(PAGE_SIZE);
                    handle.read_at(&mut page_buf, offset)?;
                    !verify_page_checksum(&page_buf)
                }
            };

            if is_torn {
                // Restore from the doublewrite copy.
                let handle = fs.open(data_path, true)?;
                let mut aligned = AlignedBuffer::zeroed(PAGE_SIZE);
                let copy_len = dw_image.len().min(PAGE_SIZE);
                aligned[..copy_len].copy_from_slice(&dw_image[..copy_len]);
                handle.write_at(&aligned, offset)?;
                handle.sync_data()?;
                restored += 1;
            }
        }

        Ok(restored)
    }

    /// Truncate the doublewrite file to zero bytes after the final write
    /// completed successfully.
    pub fn clear(&self, fs: &dyn FileSystem) -> io::Result<()> {
        let handle = fs.open(&self.path, true)?;
        handle.set_len(0)?;
        handle.sync_data()?;
        Ok(())
    }

    /// Read all `(page_id, image)` entries from the doublewrite file.
    ///
    /// Returns an empty [`Vec`] if the file is empty or malformed.
    pub fn read_entries(&self, fs: &dyn FileSystem) -> io::Result<Vec<(u64, Vec<u8>)>> {
        if !fs.exists(&self.path) {
            return Ok(vec![]);
        }
        let handle = fs.open(&self.path, false)?;
        let file_len = handle.len()? as usize;
        if file_len < 4 {
            return Ok(vec![]);
        }

        let mut raw = vec![0u8; file_len];
        handle.read_at(&mut raw, 0)?;

        let count = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let expected_len = 4 + count * ENTRY_SIZE;
        if file_len < expected_len {
            // Truncated or corrupt — return empty rather than panic.
            return Ok(vec![]);
        }

        let mut entries = Vec::with_capacity(count);
        let mut off = 4usize;
        for _ in 0..count {
            let page_id = u64::from_be_bytes([
                raw[off],     raw[off + 1], raw[off + 2], raw[off + 3],
                raw[off + 4], raw[off + 5], raw[off + 6], raw[off + 7],
            ]);
            off += 8;
            let image = raw[off..off + PAGE_SIZE].to_vec();
            off += PAGE_SIZE;
            entries.push((page_id, image));
        }

        Ok(entries)
    }
}

// ── CRC helpers ───────────────────────────────────────────────────────────────

/// Verify the CRC32C checksum stored in the page header.
///
/// Delegates to [`SlottedPage::verify_checksum`] so that byte-order handling
/// is consistent with the rest of the storage layer.  A page filled entirely
/// with zeroes is considered valid (never-written page, checksum == 0).
fn verify_page_checksum(page: &[u8]) -> bool {
    use crate::io::AlignedBuffer;
    use crate::storage::page::{SlottedPage, PAGE_SIZE};

    if page.len() < PAGE_SIZE {
        return false;
    }

    // All-zero page: treat as valid (never-written, checksum field is 0).
    if page.iter().all(|&b| b == 0) {
        return true;
    }

    // Wrap in an AlignedBuffer for SlottedPage to reuse its verify logic.
    let mut aligned = AlignedBuffer::zeroed(PAGE_SIZE);
    aligned[..PAGE_SIZE].copy_from_slice(&page[..PAGE_SIZE]);
    let sp = SlottedPage::new(aligned);
    sp.verify_checksum()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use crate::storage::page::{PageType, SlottedPage};

    fn make_page(page_id: u64) -> Vec<u8> {
        let mut p = SlottedPage::init(page_id, PageType::SlottedData);
        p.update_checksum();
        p.buf.to_vec()
    }

    #[test]
    fn write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let path = dir.path().join("dw");
        let dw = DoubleWriteBuffer::open(path, &fs).unwrap();

        let page0 = make_page(0);
        let page1 = make_page(1);
        let pages: Vec<(u64, &[u8])> = vec![(0, &page0), (1, &page1)];
        dw.write_batch(&pages, &fs).unwrap();

        let entries = dw.read_entries(&fs).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, 0);
        assert_eq!(entries[1].0, 1);
        assert_eq!(entries[0].1, page0);
        assert_eq!(entries[1].1, page1);
    }

    #[test]
    fn clear_empties_file() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let path = dir.path().join("dw");
        let dw = DoubleWriteBuffer::open(path, &fs).unwrap();

        let page0 = make_page(0);
        dw.write_batch(&[(0, &page0)], &fs).unwrap();
        dw.clear(&fs).unwrap();

        let entries = dw.read_entries(&fs).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn recover_torn_page_restored() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let dw_path = dir.path().join("dw");
        let data_path = dir.path().join("data.db");

        let dw = DoubleWriteBuffer::open(dw_path, &fs).unwrap();

        // Write a valid page into the doublewrite buffer.
        let good_page = make_page(2);
        dw.write_batch(&[(2, &good_page)], &fs).unwrap();

        // Write a corrupt/torn page into the final location.
        let handle = fs.open(&data_path, true).unwrap();
        let torn = vec![0xFFu8; PAGE_SIZE]; // invalid checksum
        handle.write_at(&torn, 2 * PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();

        let restored = dw.recover_torn_pages(&data_path, &fs).unwrap();
        assert_eq!(restored, 1);

        // Verify the restored page matches the doublewrite copy.
        let handle = fs.open(&data_path, false).unwrap();
        let mut restored_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut restored_buf, 2 * PAGE_SIZE as u64).unwrap();
        assert_eq!(&restored_buf[..], &good_page[..]);
    }

    #[test]
    fn no_recovery_needed_for_valid_page() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let dw_path = dir.path().join("dw");
        let data_path = dir.path().join("data.db");

        let dw = DoubleWriteBuffer::open(dw_path, &fs).unwrap();

        let page = make_page(0);
        dw.write_batch(&[(0, &page)], &fs).unwrap();

        // Write the same valid page to the final location.
        let handle = fs.open(&data_path, true).unwrap();
        handle.write_at(&page, 0).unwrap();
        handle.sync_data().unwrap();

        let restored = dw.recover_torn_pages(&data_path, &fs).unwrap();
        assert_eq!(restored, 0, "valid page must not be restored");
    }

    #[test]
    fn empty_doublewrite_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let dw_path = dir.path().join("dw");
        let data_path = dir.path().join("data.db");

        let dw = DoubleWriteBuffer::open(dw_path, &fs).unwrap();
        let restored = dw.recover_torn_pages(&data_path, &fs).unwrap();
        assert_eq!(restored, 0);
    }

    #[test]
    fn capacity_limit_respected() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let dw_path = dir.path().join("dw");
        let dw = DoubleWriteBuffer::open(dw_path, &fs).unwrap();

        // Build more than DOUBLEWRITE_CAPACITY pages.
        let pages: Vec<Vec<u8>> = (0..DOUBLEWRITE_CAPACITY + 10)
            .map(|i| make_page(i as u64))
            .collect();
        let page_refs: Vec<(u64, &[u8])> = pages
            .iter()
            .enumerate()
            .map(|(i, p)| (i as u64, p.as_slice()))
            .collect();

        dw.write_batch(&page_refs, &fs).unwrap();

        let entries = dw.read_entries(&fs).unwrap();
        assert_eq!(entries.len(), DOUBLEWRITE_CAPACITY);
    }
}
