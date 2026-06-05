use crate::buffer::pool::BufferPool;
use crate::io::FileSystem;
use crate::wal::record::{RecordType, WalRecord};
use crate::wal::writer::WalWriter;
use std::io;
use std::sync::Arc;

/// A fuzzy checkpoint that never blocks writers (Task 151).
///
/// The protocol:
/// 1. Write `BEGIN_CHECKPOINT` WAL record; note its LSN as the checkpoint LSN.
/// 2. Flush all dirty pages whose `rec_lsn ≤ checkpoint_lsn`.
/// 3. Write `END_CHECKPOINT` WAL record whose payload encodes the remaining
///    dirty-page table (pages not yet flushed) and the active transaction table
///    (empty placeholder until MVCC is implemented).
/// 4. Persist the checkpoint LSN to the superblock so that ARIES ANALYSIS can
///    start from this point rather than the WAL beginning.
/// 5. Advance the `wal-current` symlink.
///
/// The checkpoint is **fuzzy** because writers are not blocked during step 2:
/// new WAL records may be appended while pages are being flushed.  ARIES
/// handles this correctly because the `END_CHECKPOINT` record captures any
/// pages that were dirty but not yet flushed when the checkpoint ended.
pub struct Checkpoint;

impl Checkpoint {
    /// Perform a fuzzy checkpoint.
    ///
    /// Returns the LSN of the `CheckpointBegin` record so the caller can
    /// persist it to the superblock's `last_checkpoint_lsn` field.
    ///
    /// `wal_writer` is flushed before and after the checkpoint so all
    /// checkpoint records are durable before this call returns.
    pub fn run(
        pool: &BufferPool,
        fs: Arc<dyn FileSystem>,
        wal_writer: &mut WalWriter,
    ) -> io::Result<u64> {
        // Step 1: write BEGIN_CHECKPOINT and record its LSN.
        let begin_rec = WalRecord::new(RecordType::CheckpointBegin, 0, 0, 0, vec![]);
        let ckpt_lsn = wal_writer.append(fs.as_ref(), begin_rec)?;
        wal_writer.sync(fs.as_ref())?;

        // Step 2: flush dirty pages with rec_lsn <= checkpoint LSN.
        let candidates = pool.dirty_candidates();
        for fid in candidates {
            let frame = pool.frame(fid);
            let rec_lsn = frame
                .desc
                .rec_lsn
                .load(std::sync::atomic::Ordering::Relaxed);
            if rec_lsn != u64::MAX && rec_lsn <= ckpt_lsn {
                pool.flush_single_frame(fs.as_ref(), fid)?;
            }
        }

        // Step 3: build END_CHECKPOINT payload.
        //
        // Payload layout:
        //   [0..4]        dirty_page_count: u32 (big-endian)
        //   [4..4+N*16]   N entries of (page_id: u64, rec_lsn: u64)
        //   [4+N*16..]    active_tx_count: u32 = 0 (placeholder)
        //
        // This matches the format expected by `seed_dpt_from_checkpoint` in
        // `src/wal/aries.rs` so that ANALYSIS correctly seeds the DPT from
        // this record.
        // Only pages with a REAL rec_lsn belong in the DPT.  A frame still
        // carrying the u64::MAX sentinel (a page with no WAL dependency) is
        // neither flushed by the loop above nor a valid REDO start point, so
        // encoding it would make ANALYSIS seed the DPT with a nonsensical
        // rec_lsn and defeat the recovery bound (finding M24).
        let entries: Vec<(u64, u64)> = pool
            .dirty_candidates()
            .into_iter()
            .filter_map(|fid| {
                let frame = pool.frame(fid);
                let rec_lsn = frame
                    .desc
                    .rec_lsn
                    .load(std::sync::atomic::Ordering::Relaxed);
                if rec_lsn == u64::MAX {
                    return None;
                }
                let page_id = frame
                    .desc
                    .page_id
                    .load(std::sync::atomic::Ordering::Relaxed);
                Some((page_id, rec_lsn))
            })
            .collect();
        let mut payload = Vec::new();
        payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (page_id, rec_lsn) in entries {
            payload.extend_from_slice(&page_id.to_be_bytes());
            payload.extend_from_slice(&rec_lsn.to_be_bytes());
        }
        // Active transaction count = 0 (MVCC not yet implemented).
        payload.extend_from_slice(&0u32.to_be_bytes());

        let end_rec = WalRecord::new(RecordType::CheckpointEnd, 0, 0, 0, payload);
        wal_writer.append(fs.as_ref(), end_rec)?;
        wal_writer.sync(fs.as_ref())?;

        // Step 5: advance the wal-current symlink.
        wal_writer.update_symlink(fs.as_ref())?;

        // Return the checkpoint LSN so the caller can update the superblock's
        // `last_checkpoint_lsn` field and persist it to durable storage.
        Ok(ckpt_lsn)
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
            f.set_len(file_pages * crate::storage::page::PAGE_SIZE as u64)
                .unwrap();
            f.flush().unwrap();
        }
        let handle = fs.open(&data_path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle
                .write_at(&page.buf, pid * crate::storage::page::PAGE_SIZE as u64)
                .unwrap();
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

        let ckpt_lsn = Checkpoint::run(&pool, fs.clone(), &mut wal).unwrap();
        assert!(ckpt_lsn > 0, "checkpoint LSN must be non-zero");

        // After checkpoint the page should be clean.
        let mut found = false;
        for frame in pool.iter_frames() {
            if frame
                .desc
                .page_id
                .load(std::sync::atomic::Ordering::Relaxed)
                == 1
            {
                assert!(!frame.desc.dirty.load(std::sync::atomic::Ordering::Relaxed));
                found = true;
                break;
            }
        }
        assert!(found);
    }

    // ── Task 151: checkpoint LSN returned and WAL record contains DPT ─────────

    #[test]
    fn checkpoint_lsn_is_in_wal_and_dpt_is_encoded() {
        use crate::storage::page::{PageType, SlottedPage};
        use crate::wal::record::WalRecord;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("rgraph.db");
        let wal_dir = dir.path().join("wal");
        let fs = Arc::new(PosixFileSystem::new(false));

        // Pre-allocate data file.
        let file_pages = 32u64;
        {
            let mut f = std::fs::File::create(&data_path).unwrap();
            f.set_len(file_pages * crate::storage::page::PAGE_SIZE as u64)
                .unwrap();
            f.flush().unwrap();
        }
        let handle = fs.open(&data_path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle
                .write_at(&page.buf, pid * crate::storage::page::PAGE_SIZE as u64)
                .unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        let pool = BufferPool::new(4, data_path);
        let mut wal = WalWriter::open(wal_dir.clone(), fs.as_ref()).unwrap();

        // Mark page 2 as dirty.
        {
            let mut guard = pool.fix_page(fs.as_ref(), 2).unwrap();
            guard.buf_mut()[0] = 0xCC;
            guard.set_dirty(10);
        }

        let ckpt_lsn = Checkpoint::run(&pool, fs.clone(), &mut wal).unwrap();
        assert!(ckpt_lsn > 0, "checkpoint LSN must be non-zero");

        // Verify the WAL contains a CheckpointEnd record with the DPT encoded.
        let wal_path = wal_dir.join("wal-000000000");
        let raw = std::fs::read(&wal_path).unwrap();
        let mut offset = 0;
        let mut found_end = false;
        while offset < raw.len() {
            if let Some((rec, size)) = WalRecord::decode(&raw, offset) {
                if rec.record_type == crate::wal::record::RecordType::CheckpointEnd {
                    found_end = true;
                    // Payload must encode at least the count field.
                    assert!(rec.payload.len() >= 4, "CheckpointEnd must have payload");
                }
                offset += size;
            } else {
                break;
            }
        }
        assert!(found_end, "CheckpointEnd record must be present in WAL");
    }

    /// Regression gate for finding M24 (Task 202, 2026-06-05): the CheckpointEnd
    /// DPT must contain only real rec_lsn values (never the u64::MAX sentinel),
    /// and pages with rec_lsn <= ckpt_lsn must be flushed clean.
    #[test]
    fn checkpoint_dpt_excludes_sentinel_rec_lsn() {
        use crate::storage::page::{PageType, SlottedPage};
        use crate::wal::record::WalRecord;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("rgraph.db");
        let wal_dir = dir.path().join("wal");
        let fs = Arc::new(PosixFileSystem::new(false));

        let file_pages = 32u64;
        {
            let mut f = std::fs::File::create(&data_path).unwrap();
            f.set_len(file_pages * crate::storage::page::PAGE_SIZE as u64)
                .unwrap();
            f.flush().unwrap();
        }
        let handle = fs.open(&data_path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle
                .write_at(&page.buf, pid * crate::storage::page::PAGE_SIZE as u64)
                .unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        let pool = BufferPool::new(8, data_path);
        let mut wal = WalWriter::open(wal_dir.clone(), fs.as_ref()).unwrap();

        // Page 1: rec_lsn = 1 (<= ckpt_lsn) → must be flushed clean.
        {
            let mut g = pool.fix_page(fs.as_ref(), 1).unwrap();
            g.buf_mut()[64] = 0xAA;
            g.set_dirty(1);
        }
        // Page 2: the u64::MAX sentinel → must be EXCLUDED from the DPT.
        {
            let mut g = pool.fix_page(fs.as_ref(), 2).unwrap();
            g.buf_mut()[64] = 0xBB;
            g.set_dirty(u64::MAX);
        }
        // Page 3: a future rec_lsn (> ckpt_lsn) → stays dirty, belongs in the DPT.
        {
            let mut g = pool.fix_page(fs.as_ref(), 3).unwrap();
            g.buf_mut()[64] = 0xCC;
            g.set_dirty(1_000_000);
        }

        let ckpt_lsn = Checkpoint::run(&pool, fs.clone(), &mut wal).unwrap();

        // Page 1 (rec_lsn <= ckpt_lsn) must be clean after the checkpoint.
        let page1_clean = pool
            .iter_frames()
            .find(|f| f.desc.page_id.load(Ordering::Relaxed) == 1)
            .map(|f| !f.desc.dirty.load(Ordering::Relaxed))
            .unwrap_or(false);
        assert!(page1_clean, "a page with rec_lsn <= ckpt_lsn must be flushed clean");

        // Decode the CheckpointEnd DPT.
        let raw = std::fs::read(wal_dir.join("wal-000000000")).unwrap();
        let mut offset = 0;
        let mut dpt: Vec<(u64, u64)> = Vec::new();
        let mut found_end = false;
        while offset < raw.len() {
            let Some((rec, size)) = WalRecord::decode(&raw, offset) else {
                break;
            };
            if rec.record_type == crate::wal::record::RecordType::CheckpointEnd {
                found_end = true;
                let p = &rec.payload;
                let count = u32::from_be_bytes(p[0..4].try_into().unwrap()) as usize;
                for i in 0..count {
                    let base = 4 + i * 16;
                    let page_id = u64::from_be_bytes(p[base..base + 8].try_into().unwrap());
                    let rec_lsn = u64::from_be_bytes(p[base + 8..base + 16].try_into().unwrap());
                    dpt.push((page_id, rec_lsn));
                }
            }
            offset += size;
        }
        assert!(found_end, "CheckpointEnd record must be present");
        assert!(ckpt_lsn < 1_000_000, "the future rec_lsn must exceed ckpt_lsn");

        // No DPT entry may carry the sentinel; page 2 (u64::MAX) is excluded.
        assert!(
            !dpt.iter().any(|(_, lsn)| *lsn == u64::MAX),
            "the DPT must not encode the u64::MAX sentinel, got {dpt:?}"
        );
        assert!(
            !dpt.iter().any(|(pid, _)| *pid == 2),
            "the sentinel page 2 must not appear in the DPT, got {dpt:?}"
        );
        // Page 3 (real future rec_lsn) belongs in the DPT.
        assert!(
            dpt.contains(&(3, 1_000_000)),
            "a page with a real rec_lsn > ckpt_lsn must be in the DPT, got {dpt:?}"
        );
    }
}
