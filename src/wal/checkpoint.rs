use crate::buffer::pool::BufferPool;
use crate::io::FileSystem;
use crate::wal::record::{RecordType, WalRecord};
use crate::wal::writer::WalWriter;
use std::io;
use std::sync::Arc;

/// A fuzzy checkpoint that never blocks writers.
///
/// The protocol:
/// 1. Write BEGIN_CHECKPOINT WAL record.
/// 2. Flush all dirty pages with rec_lsn <= checkpoint LSN.
/// 3. Write END_CHECKPOINT WAL record with dirty page table + active tx table.
/// 4. Update superblock and truncate old WAL segments.
pub struct Checkpoint;

impl Checkpoint {
    /// Perform a fuzzy checkpoint.
    ///
    /// `wal_writer` must be flushed before this call so that the checkpoint
    /// LSN is stable on disk.
    pub fn run(
        pool: &BufferPool,
        fs: Arc<dyn FileSystem>,
        wal_writer: &mut WalWriter,
    ) -> io::Result<()> {
        // Step 1: write BEGIN_CHECKPOINT.
        let begin_rec = WalRecord::new(RecordType::CheckpointBegin, 0, 0, 0, vec![]);
        let ckpt_lsn = wal_writer.append(fs.as_ref(), begin_rec)?;
        wal_writer.sync(fs.as_ref())?;

        // Step 2: flush dirty pages with rec_lsn <= checkpoint LSN.
        let candidates = pool.dirty_candidates();
        for fid in candidates {
            let frame = pool.frame(fid);
            let rec_lsn = frame.desc.rec_lsn.load(std::sync::atomic::Ordering::Relaxed);
            if rec_lsn != u64::MAX && rec_lsn <= ckpt_lsn {
                pool.flush_single_frame(fs.as_ref(), fid)?;
            }
        }

        // Step 3: build END_CHECKPOINT payload.
        // Payload format (simple binary): count of dirty pages, then (page_id, rec_lsn) pairs.
        // Active transaction table is empty for now (Sprint 5 will add transactions).
        let mut payload = Vec::new();
        let dirty_pages = pool.dirty_candidates();
        let count = dirty_pages.len() as u32;
        payload.extend_from_slice(&count.to_be_bytes());
        for fid in dirty_pages {
            let frame = pool.frame(fid);
            let page_id = frame.desc.page_id.load(std::sync::atomic::Ordering::Relaxed);
            let rec_lsn = frame.desc.rec_lsn.load(std::sync::atomic::Ordering::Relaxed);
            payload.extend_from_slice(&page_id.to_be_bytes());
            payload.extend_from_slice(&rec_lsn.to_be_bytes());
        }
        // Active transaction count = 0 (placeholder for Sprint 5).
        payload.extend_from_slice(&0u32.to_be_bytes());

        let end_rec = WalRecord::new(RecordType::CheckpointEnd, 0, 0, 0, payload);
        wal_writer.append(fs.as_ref(), end_rec)?;
        wal_writer.sync(fs.as_ref())?;

        // Step 4: update superblock and truncate old WAL.
        // For now we just advance the symlink; full segment truncation will
        // be added when multi-segment WAL rotation is implemented.
        wal_writer.update_symlink(fs.as_ref())?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::pool::BufferPool;
    use crate::io::posix::PosixFileSystem;
    use std::io::Write;

    #[test]
    fn checkpoint_flushes_and_marks() {
        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("rgraph.db");
        let wal_dir = dir.path().join("wal");
        let fs = Arc::new(PosixFileSystem::new(false));

        // Pre-allocate data file and initialize pages with valid checksums.
        let file_pages = 64u64;
        {
            let mut f = std::fs::File::create(&data_path).unwrap();
            f.set_len(file_pages * crate::storage::page::PAGE_SIZE as u64).unwrap();
            f.flush().unwrap();
        }
        let handle = fs.open(&data_path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * crate::storage::page::PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        let pool = BufferPool::new(4, data_path);
        let mut wal = WalWriter::open(wal_dir, fs.as_ref()).unwrap();

        {
            let mut guard = pool.fix_page(fs.as_ref(), 1).unwrap();
            guard.buf_mut()[0] = 0xAB;
            guard.set_dirty(1);
        }

        Checkpoint::run(&pool, fs.clone(), &mut wal).unwrap();

        // After checkpoint the page should be clean.
        let mut found = false;
        for frame in pool.iter_frames() {
            if frame.desc.page_id.load(std::sync::atomic::Ordering::Relaxed) == 1 {
                assert!(!frame.desc.dirty.load(std::sync::atomic::Ordering::Relaxed));
                found = true;
                break;
            }
        }
        assert!(found);
    }
}
