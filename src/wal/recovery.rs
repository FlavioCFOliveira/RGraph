use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::page::{PageHeader, PageId, PAGE_SIZE};
use crate::wal::record::{RecordType, WalRecord};
use std::io;
use std::path::Path;

/// Scan WAL from `start_lsn`, validate every record, and invoke
/// `replay` for each page-level record.  Returns the LSN of the last
/// valid record, or `None` if the WAL is empty.
///
/// Invalid records cause a truncation at that point.
pub fn recover<F>(
    fs: &dyn FileSystem,
    wal_path: &Path,
    start_lsn: u64,
    mut replay: F,
) -> io::Result<Option<u64>>
where
    F: FnMut(PageId, &[u8], u64) -> io::Result<()>,
{
    if !fs.exists(wal_path) {
        return Ok(None);
    }
    let handle = fs.open(wal_path, false)?;
    let len = handle.len()?;
    if len == 0 {
        return Ok(None);
    }
    let mut buf = vec![0u8; len as usize];
    handle.read_at(&mut buf, 0)?;

    let mut offset = start_lsn as usize;
    let mut last_valid_lsn: Option<u64> = None;

    while offset < buf.len() {
        match WalRecord::decode(&buf, offset) {
            Some((rec, size)) => {
                let lsn = rec.lsn;
                match rec.record_type {
                    RecordType::PageInsert
                    | RecordType::PageUpdate
                    | RecordType::PageFree
                    | RecordType::BitmapUpdate
                        if rec.payload.len() >= 8 =>
                    {
                        // Payload: first 8 bytes = page_id, rest = after-image.
                        let page_id = u64::from_be_bytes([
                            rec.payload[0], rec.payload[1], rec.payload[2], rec.payload[3],
                            rec.payload[4], rec.payload[5], rec.payload[6], rec.payload[7],
                        ]);
                        let after_image = &rec.payload[8..];
                        replay(page_id, after_image, lsn)?;
                    }
                    _ => {
                        // Transactional / checkpoint records: nothing to replay at
                        // page level in redo-only recovery.
                    }
                }
                last_valid_lsn = Some(lsn);
                offset += size;
            }
            None => {
                // First invalid record encountered: truncate here.
                break;
            }
        }
    }

    // If we truncated, resize the WAL file.
    if offset < buf.len() {
        let handle = fs.open(wal_path, false)?;
        handle.set_len(offset as u64)?;
        handle.sync_data()?;
    }

    Ok(last_valid_lsn)
}

/// Convenience replay function that writes the after-image directly to
/// the data file if the page’s on-disk PageLSN is < `record_lsn`.
///
/// `data_path` must point to the database file containing the pages.
pub fn simple_page_replay(
    fs: &dyn FileSystem,
    data_path: &Path,
    page_id: PageId,
    after_image: &[u8],
    record_lsn: u64,
) -> io::Result<()> {
    let offset = page_id * PAGE_SIZE as u64;
    let handle = fs.open(data_path, false)?;

    // Read existing header to check page_lsn.
    let mut hdr_buf = [0u8; size_of::<PageHeader>()];
    let mut page_lsn = 0u64;
    if handle.read_at(&mut hdr_buf, offset).is_ok() {
        // If the page exists, extract its LSN (first 8 bytes of header).
        page_lsn = u64::from_be_bytes([
            hdr_buf[0], hdr_buf[1], hdr_buf[2], hdr_buf[3],
            hdr_buf[4], hdr_buf[5], hdr_buf[6], hdr_buf[7],
        ]);
    }

    if page_lsn >= record_lsn {
        // Already flushed; skip idempotent redo.
        return Ok(());
    }

    // Write the after-image.  For `PageUpdate` we expect the payload to
    // be a full page image (or a partial one starting at offset 0).
    let mut aligned = AlignedBuffer::zeroed(PAGE_SIZE);
    let copy_len = after_image.len().min(PAGE_SIZE);
    aligned[..copy_len].copy_from_slice(&after_image[..copy_len]);

    // Patch the page_lsn field so future recoveries know this page is
    // up-to-date.
    aligned[0..8].copy_from_slice(&record_lsn.to_be_bytes());

    handle.write_at(&aligned, offset)?;
    handle.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use crate::wal::record::{RecordType, WalRecord};
    use crate::wal::writer::WalWriter;
    #[test]
    fn redo_applies_after_crash() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let wal_dir = dir.path().join("wal");
        let data_path = dir.path().join("rgraph.db");

        // Pre-create data file with page 2 initialised.
        {
            let handle = fs.open(&data_path, true).unwrap();
            let page = crate::storage::page::SlottedPage::init(2, crate::storage::page::PageType::SlottedData);
            handle.write_at(&page.buf, 2 * PAGE_SIZE as u64).unwrap();
            handle.sync_data().unwrap();
        }

        // Write a WAL record that mutates page 2.
        let mut writer = WalWriter::open(wal_dir.clone(), &fs).unwrap();
        let mut new_page = crate::storage::page::SlottedPage::init(2, crate::storage::page::PageType::SlottedData);
        let idx = new_page.insert(b"recovered").unwrap();
        new_page.update_checksum();
        let payload = {
            let mut p = 2u64.to_be_bytes().to_vec();
            p.extend_from_slice(&new_page.buf);
            p
        };
        let rec = WalRecord::new(RecordType::PageUpdate, 1, 0, 0, payload);
        writer.append(&fs, rec).unwrap();
        writer.sync(&fs).unwrap();

        // Recover.
        let wal_path = wal_dir.join("wal-000000000");
        let last_lsn = recover(&fs, &wal_path, 0, |pid, img, lsn| {
            simple_page_replay(&fs, &data_path, pid, img, lsn)
        })
        .unwrap();
        assert!(last_lsn.is_some());

        // Verify page 2 now contains the record.
        let handle = fs.open(&data_path, false).unwrap();
        let mut buf = crate::io::AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 2 * PAGE_SIZE as u64).unwrap();
        let page = crate::storage::page::SlottedPage::new(buf);
        assert_eq!(page.read(idx).unwrap(), b"recovered");
    }

    #[test]
    fn truncate_on_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let wal_dir = dir.path().join("wal");
        let mut writer = WalWriter::open(wal_dir.clone(), &fs).unwrap();

        let rec = WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]);
        writer.append(&fs, rec).unwrap();
        writer.sync(&fs).unwrap();

        // Corrupt the tail of the segment.
        let wal_path = wal_dir.join("wal-000000000");
        {
            let handle = fs.open(&wal_path, false).unwrap();
            let len = handle.len().unwrap();
            let mut buf = vec![0u8; len as usize];
            handle.read_at(&mut buf, 0).unwrap();
            let end = buf.len();
            buf[end - 4..].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
            let handle = fs.open(&wal_path, false).unwrap();
            handle.write_at(&buf, 0).unwrap();
            handle.sync_data().unwrap();
        }

        let last = recover(&fs, &wal_path, 0, |_pid, _img, _lsn| Ok(())).unwrap();
        // The valid record was truncated because the end magic is wrong,
        // so the WAL file should now be empty.
        assert!(last.is_none());
        let handle = fs.open(&wal_path, false).unwrap();
        assert_eq!(handle.len().unwrap(), 0);
    }
}
