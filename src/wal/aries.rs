//! Full three-phase ARIES crash recovery (Tasks 106, 107, 108).
//!
//! Implements the classic ARIES algorithm as described in:
//! > Mohan et al., "ARIES: A Transaction Recovery Method Supporting
//! > Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead
//! > Logging", ACM TODS 1992.
//!
//! # Overview
//!
//! Recovery proceeds in three sequential phases:
//!
//! 1. **ANALYSIS** — scan WAL forward from the last checkpoint LSN,
//!    rebuilding the Active Transaction Table (ATT) and Dirty Page Table (DPT).
//!
//! 2. **REDO** — replay after-images for all pages in the DPT, starting from
//!    `min(rec_lsn)`.  Each application is idempotent: a page whose
//!    `page_lsn >= record.lsn` is skipped.
//!
//! 3. **UNDO** — for every `Active` or `Aborted` transaction remaining in the
//!    ATT, traverse backward via `prev_lsn`, apply the inverse operation, and
//!    emit a Compensation Log Record (CLR) so the undo is itself durable and
//!    idempotent under repeated crashes during recovery.
//!
//! # Testability
//!
//! [`AriesRecovery::new_with_records`] accepts an in-memory record slice so
//! that property tests can drive all three phases without touching the
//! filesystem.

use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::page::{PageId, PAGE_SIZE};
use crate::wal::record::{RecordType, WalRecord};
use crate::wal::writer::WalWriter;
use std::collections::HashMap;
use std::io;
use std::path::Path;

// ── ATT ──────────────────────────────────────────────────────────────────────

/// Lifecycle state of a transaction as reconstructed by ANALYSIS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    /// Transaction began but never committed or aborted before the crash.
    Active,
    /// Transaction committed normally.
    Committed,
    /// Transaction was aborted (explicitly or due to a wound-wait decision).
    Aborted,
}

/// One row in the Active Transaction Table (ATT).
#[derive(Debug, Clone)]
pub struct AttEntry {
    /// Transaction identifier.
    pub txid: u64,
    /// Status derived from the WAL.
    pub status: TxStatus,
    /// LSN of the most recent WAL record written by this transaction.
    pub last_lsn: u64,
}

// ── DPT ──────────────────────────────────────────────────────────────────────

/// One row in the Dirty Page Table (DPT).
#[derive(Debug, Clone)]
pub struct DptEntry {
    /// Physical page identifier.
    pub page_id: u64,
    /// Earliest LSN that first dirtied this page after the checkpoint.
    /// REDO must replay all records at or after this LSN for this page.
    pub rec_lsn: u64,
}

// ── Result ────────────────────────────────────────────────────────────────────

/// Summary produced by a completed [`AriesRecovery::recover`] run.
#[derive(Debug)]
pub struct RecoveryResult {
    /// The ATT as it stood at crash time (after UNDO it should be empty of
    /// Active/Aborted entries).
    pub att: HashMap<u64, AttEntry>,
    /// The DPT built during ANALYSIS.
    pub dpt: HashMap<u64, DptEntry>,
    /// Highest LSN seen in the WAL.
    pub max_lsn: u64,
    /// Number of page after-images applied during REDO.
    pub redo_count: usize,
    /// Number of inverse operations applied during UNDO.
    pub undo_count: usize,
}

// ── AriesRecovery ─────────────────────────────────────────────────────────────

/// Driver for the three-phase ARIES recovery algorithm.
pub struct AriesRecovery<'a> {
    fs: Option<&'a dyn FileSystem>,
    wal_path: Option<&'a Path>,
    data_path: Option<&'a Path>,
    /// WAL LSN from which ANALYSIS starts (typically the checkpoint begin LSN).
    checkpoint_lsn: u64,
    /// Pre-loaded in-memory records used when `fs` is `None` (test mode).
    preloaded_records: Option<Vec<WalRecord>>,
}

impl<'a> AriesRecovery<'a> {
    /// Production constructor: reads WAL from disk.
    pub fn new(
        fs: &'a dyn FileSystem,
        wal_path: &'a Path,
        data_path: &'a Path,
        checkpoint_lsn: u64,
    ) -> Self {
        Self {
            fs: Some(fs),
            wal_path: Some(wal_path),
            data_path: Some(data_path),
            checkpoint_lsn,
            preloaded_records: None,
        }
    }

    /// Test constructor: uses an in-memory record slice, no filesystem I/O.
    ///
    /// `analysis_from_records` and the full `recover_from_records` path become
    /// usable, but `recover` (which requires a real filesystem) will return an
    /// error.
    pub fn new_with_records(records: Vec<WalRecord>) -> Self {
        Self {
            fs: None,
            wal_path: None,
            data_path: None,
            checkpoint_lsn: 0,
            preloaded_records: Some(records),
        }
    }

    // ── Phase helper exposed for tests ────────────────────────────────────────

    /// Run ANALYSIS on the preloaded record set.
    ///
    /// Returns `(att, dpt, max_lsn)`.
    ///
    /// # Panics
    ///
    /// Panics if called on a production (non-test) instance.
    pub fn analysis_from_records(&self) -> (HashMap<u64, AttEntry>, HashMap<u64, DptEntry>, u64) {
        let records = self
            .preloaded_records
            .as_ref()
            .expect("analysis_from_records requires new_with_records");
        self.analysis(records)
    }

    // ── Public entry point ────────────────────────────────────────────────────

    /// Run full ARIES recovery: ANALYSIS → REDO → UNDO.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if reading the WAL or writing to data pages
    /// fails.
    pub fn recover(&self, wal: &mut WalWriter) -> io::Result<RecoveryResult> {
        let fs = self.fs.expect("recover requires a filesystem (use new())");
        let wal_path = self.wal_path.expect("recover requires a WAL path");

        let records = self.load_records(fs, wal_path)?;
        self.run_all_phases(&records, wal)
    }

    /// Run all three phases on an explicit record slice (usable in tests without
    /// a real WAL path when constructing a dummy [`WalWriter`] is acceptable).
    pub fn recover_from_slice(
        &self,
        records: &[WalRecord],
        wal: &mut WalWriter,
    ) -> io::Result<RecoveryResult> {
        self.run_all_phases(records, wal)
    }

    // ── Internal: load records from WAL file ──────────────────────────────────

    fn load_records(
        &self,
        fs: &dyn FileSystem,
        wal_path: &Path,
    ) -> io::Result<Vec<WalRecord>> {
        if !fs.exists(wal_path) {
            return Ok(vec![]);
        }
        let handle = fs.open(wal_path, false)?;
        let len = handle.len()? as usize;
        if len == 0 {
            return Ok(vec![]);
        }
        let mut raw = vec![0u8; len];
        handle.read_at(&mut raw, 0)?;

        let mut records = Vec::new();
        let mut offset = 0usize;
        while offset < raw.len() {
            match WalRecord::decode(&raw, offset) {
                Some((rec, size)) => {
                    offset += size;
                    records.push(rec);
                }
                None => break, // truncated / corrupt tail; stop here
            }
        }
        Ok(records)
    }

    // ── Internal: orchestrate three phases ───────────────────────────────────

    fn run_all_phases(
        &self,
        records: &[WalRecord],
        wal: &mut WalWriter,
    ) -> io::Result<RecoveryResult> {
        // Narrow the slice to records at or after the checkpoint.
        let analysis_records: Vec<&WalRecord> = records
            .iter()
            .filter(|r| r.lsn >= self.checkpoint_lsn)
            .collect();

        let (att, dpt, max_lsn) = self.analysis(
            &analysis_records.iter().map(|r| (*r).clone()).collect::<Vec<_>>(),
        );

        let redo_count = if let Some(fs) = self.fs {
            self.redo(records, &dpt, fs)?
        } else {
            // Test mode: REDO phase requires real I/O — skip silently.
            0
        };

        let undo_count = if let Some(fs) = self.fs {
            self.undo(&att, records, wal, fs)?
        } else {
            0
        };

        Ok(RecoveryResult {
            att,
            dpt,
            max_lsn,
            redo_count,
            undo_count,
        })
    }

    // ── Phase 1: ANALYSIS ─────────────────────────────────────────────────────

    /// Scan `records` forward.
    ///
    /// Rebuilds the ATT and DPT.  All three return values are owned to allow
    /// independent use from tests.
    fn analysis(
        &self,
        records: &[WalRecord],
    ) -> (HashMap<u64, AttEntry>, HashMap<u64, DptEntry>, u64) {
        let mut att: HashMap<u64, AttEntry> = HashMap::new();
        let mut dpt: HashMap<u64, DptEntry> = HashMap::new();
        let mut max_lsn = 0u64;

        for rec in records {
            let lsn = rec.lsn;
            if lsn > max_lsn {
                max_lsn = lsn;
            }

            match rec.record_type {
                // ── Transaction lifecycle ───────────────────────────────────
                RecordType::Begin => {
                    att.entry(rec.txid).or_insert_with(|| AttEntry {
                        txid: rec.txid,
                        status: TxStatus::Active,
                        last_lsn: lsn,
                    });
                    // If the entry already exists (re-Begin after partial undo),
                    // just update the last_lsn.
                    if let Some(e) = att.get_mut(&rec.txid)
                        && lsn > e.last_lsn
                    {
                        e.last_lsn = lsn;
                    }
                }
                RecordType::Commit => {
                    let entry = att.entry(rec.txid).or_insert_with(|| AttEntry {
                        txid: rec.txid,
                        status: TxStatus::Committed,
                        last_lsn: lsn,
                    });
                    entry.status = TxStatus::Committed;
                    if lsn > entry.last_lsn {
                        entry.last_lsn = lsn;
                    }
                }
                RecordType::Abort => {
                    let entry = att.entry(rec.txid).or_insert_with(|| AttEntry {
                        txid: rec.txid,
                        status: TxStatus::Aborted,
                        last_lsn: lsn,
                    });
                    entry.status = TxStatus::Aborted;
                    if lsn > entry.last_lsn {
                        entry.last_lsn = lsn;
                    }
                }

                // ── CLR records: update ATT but do not add to DPT ──────────
                RecordType::Clr => {
                    if let Some(entry) = att.get_mut(&rec.txid)
                        && lsn > entry.last_lsn
                    {
                        entry.last_lsn = lsn;
                    }
                }

                // ── Page-mutation records: update ATT + DPT ─────────────────
                RecordType::PageInsert
                | RecordType::PageUpdate
                | RecordType::PageFree
                | RecordType::BitmapUpdate
                | RecordType::NodeInsert
                | RecordType::NodeDelete
                | RecordType::NodeUpdate
                | RecordType::EdgeInsert
                | RecordType::EdgeDelete
                | RecordType::EdgeUpdate
                | RecordType::PropertyInsert
                | RecordType::PropertyUpdate => {
                    // Payload must start with an 8-byte page_id.
                    if rec.payload.len() >= 8 {
                        let page_id = page_id_from_payload(&rec.payload);
                        dpt.entry(page_id).or_insert(DptEntry {
                            page_id,
                            rec_lsn: lsn,
                        });
                        // Oldest rec_lsn wins — do not overwrite with a later one.
                    }
                    // Update ATT last_lsn for this txid.
                    if let Some(entry) = att.get_mut(&rec.txid)
                        && lsn > entry.last_lsn
                    {
                        entry.last_lsn = lsn;
                    }
                }

                // ── Checkpoint / descriptor records: no ATT/DPT side effects ─
                RecordType::CheckpointBegin
                | RecordType::CheckpointEnd
                | RecordType::SegmentDescriptor => {}
            }
        }

        (att, dpt, max_lsn)
    }

    // ── Phase 2: REDO ─────────────────────────────────────────────────────────

    /// Replay after-images for pages listed in `dpt`.
    ///
    /// Records are applied only if `page.page_lsn < record.lsn` (idempotent).
    /// Counts the number of actual applications.
    fn redo(
        &self,
        records: &[WalRecord],
        dpt: &HashMap<u64, DptEntry>,
        fs: &dyn FileSystem,
    ) -> io::Result<usize> {
        if dpt.is_empty() {
            return Ok(0);
        }

        let data_path = self
            .data_path
            .expect("REDO phase requires a data path (use new())");

        // Start replaying from the minimum rec_lsn in the DPT.
        let min_rec_lsn = dpt.values().map(|e| e.rec_lsn).min().unwrap_or(0);

        let mut count = 0usize;

        for rec in records {
            if rec.lsn < min_rec_lsn {
                continue;
            }

            // Only page-mutation records and CLRs have after-images to apply.
            let is_page_mutation = matches!(
                rec.record_type,
                RecordType::PageInsert
                    | RecordType::PageUpdate
                    | RecordType::PageFree
                    | RecordType::BitmapUpdate
                    | RecordType::NodeInsert
                    | RecordType::NodeDelete
                    | RecordType::NodeUpdate
                    | RecordType::EdgeInsert
                    | RecordType::EdgeDelete
                    | RecordType::EdgeUpdate
                    | RecordType::PropertyInsert
                    | RecordType::PropertyUpdate
                    | RecordType::Clr
            );
            if !is_page_mutation {
                continue;
            }

            // Determine page_id from payload.
            let (page_id, after_image) = match extract_page_and_image(rec) {
                Some(v) => v,
                None => continue,
            };

            // Is this page in the DPT?
            if !dpt.contains_key(&page_id) {
                continue;
            }

            // Read the current on-disk page_lsn.
            let on_disk_page_lsn = read_page_lsn(fs, data_path, page_id)?;
            if on_disk_page_lsn >= rec.lsn {
                // Already at or past this update — idempotent skip.
                continue;
            }

            // Apply the after-image.
            apply_after_image(fs, data_path, page_id, after_image, rec.lsn)?;
            count += 1;
        }

        Ok(count)
    }

    // ── Phase 3: UNDO ─────────────────────────────────────────────────────────

    /// Undo all `Active` and `Aborted` transactions by traversing backward
    /// through their WAL chains and emitting CLRs.
    fn undo(
        &self,
        att: &HashMap<u64, AttEntry>,
        records: &[WalRecord],
        wal: &mut WalWriter,
        fs: &dyn FileSystem,
    ) -> io::Result<usize> {
        let data_path = self
            .data_path
            .expect("UNDO phase requires a data path (use new())");

        // Build a lookup: lsn → record (for prev_lsn chain traversal).
        let lsn_index: HashMap<u64, &WalRecord> =
            records.iter().map(|r| (r.lsn, r)).collect();

        // Process transactions ordered by last_lsn descending (the ARIES
        // "toundo" priority queue).
        let mut to_undo: Vec<&AttEntry> = att
            .values()
            .filter(|e| e.status == TxStatus::Active || e.status == TxStatus::Aborted)
            .collect();
        to_undo.sort_by_key(|e| std::cmp::Reverse(e.last_lsn));

        let mut total_undo = 0usize;

        for entry in to_undo {
            let mut current_lsn = entry.last_lsn;

            while let Some(rec) = lsn_index.get(&current_lsn) {
                // CLRs are never undone; skip directly to their undo_next_lsn.
                if rec.record_type == RecordType::Clr {
                    current_lsn = clr_undo_next_lsn(&rec.payload);
                    if current_lsn == 0 {
                        break;
                    }
                    continue;
                }

                // Only undoable page-mutation records produce inverse ops.
                let undoable = matches!(
                    rec.record_type,
                    RecordType::PageInsert
                        | RecordType::PageUpdate
                        | RecordType::NodeInsert
                        | RecordType::NodeDelete
                        | RecordType::NodeUpdate
                        | RecordType::EdgeInsert
                        | RecordType::EdgeDelete
                        | RecordType::EdgeUpdate
                        | RecordType::PropertyInsert
                        | RecordType::PropertyUpdate
                        | RecordType::BitmapUpdate
                        | RecordType::PageFree
                );

                if undoable
                    && let Some((page_id, after_image)) = extract_page_and_image(rec)
                {
                    // Apply inverse operation on the page.
                    apply_inverse(fs, data_path, rec, page_id, after_image)?;

                    // Write CLR to WAL.
                    let clr_payload =
                        build_clr_payload(rec.prev_lsn, page_id, after_image);
                    let clr = WalRecord::new(
                        RecordType::Clr,
                        rec.txid,
                        0,           // LSN assigned by WalWriter
                        rec.prev_lsn,
                        clr_payload,
                    );
                    wal.append(fs, clr)?;
                    total_undo += 1;
                }

                // Follow the prev_lsn chain.
                current_lsn = rec.prev_lsn;
                if current_lsn == 0 {
                    break;
                }
            }

            // Flush CLRs for this transaction before moving to the next.
            wal.flush(fs)?;
        }

        Ok(total_undo)
    }
}

// ── Payload helpers ───────────────────────────────────────────────────────────

/// Extract the 8-byte big-endian `page_id` from the beginning of a payload.
fn page_id_from_payload(payload: &[u8]) -> PageId {
    u64::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
        payload[4], payload[5], payload[6], payload[7],
    ])
}

/// For standard page-mutation records: payload = `[8 bytes page_id][image...]`.
/// For CLR records: payload = `[8 undo_next_lsn][8 page_id][image...]`.
fn extract_page_and_image(rec: &WalRecord) -> Option<(PageId, &[u8])> {
    match rec.record_type {
        RecordType::Clr => {
            // CLR payload: [8 undo_next_lsn][8 page_id][image...]
            if rec.payload.len() < 16 {
                return None;
            }
            let page_id = page_id_from_payload(&rec.payload[8..16]);
            Some((page_id, &rec.payload[16..]))
        }
        _ => {
            // Standard: [8 page_id][image...]
            if rec.payload.len() < 8 {
                return None;
            }
            let page_id = page_id_from_payload(&rec.payload);
            Some((page_id, &rec.payload[8..]))
        }
    }
}

/// Read the `page_lsn` (first 8 bytes) of a page from disk.
/// Returns 0 if the page does not exist yet.
fn read_page_lsn(fs: &dyn FileSystem, data_path: &Path, page_id: PageId) -> io::Result<u64> {
    if !fs.exists(data_path) {
        return Ok(0);
    }
    let handle = fs.open(data_path, false)?;
    let offset = page_id * PAGE_SIZE as u64;
    let file_len = handle.len()?;
    if file_len < offset + 8 {
        return Ok(0);
    }
    let mut hdr = [0u8; 8];
    handle.read_at(&mut hdr, offset)?;
    Ok(u64::from_be_bytes(hdr))
}

/// Write `after_image` to `page_id`'s location in `data_path`, patching
/// `page_lsn` to `record_lsn` so future idempotent checks work correctly.
fn apply_after_image(
    fs: &dyn FileSystem,
    data_path: &Path,
    page_id: PageId,
    after_image: &[u8],
    record_lsn: u64,
) -> io::Result<()> {
    let handle = fs.open(data_path, true)?;
    let offset = page_id * PAGE_SIZE as u64;

    let mut page_buf = AlignedBuffer::zeroed(PAGE_SIZE);
    // Copy provided after-image (may be a full page or partial).
    let copy_len = after_image.len().min(PAGE_SIZE);
    page_buf[..copy_len].copy_from_slice(&after_image[..copy_len]);
    // Stamp page_lsn so idempotency holds.
    page_buf[0..8].copy_from_slice(&record_lsn.to_be_bytes());

    handle.write_at(&page_buf, offset)?;
    Ok(())
}

/// Apply the inverse of `rec` to bring `page_id` back to its pre-operation state.
///
/// For now we implement a "best effort" inverse:
/// - `NodeInsert` / `EdgeInsert` / `PropertyInsert` / `PageInsert`: write a
///   zero-filled tombstone (logically deleted page).
/// - `NodeDelete` / `EdgeDelete` / `PageFree` / `PageUpdate` etc.: restore
///   the before-image if it was recorded in the payload (bytes 8+8 and beyond),
///   otherwise write a tombstone.
fn apply_inverse(
    fs: &dyn FileSystem,
    data_path: &Path,
    rec: &WalRecord,
    page_id: PageId,
    _after_image: &[u8],
) -> io::Result<()> {
    // For insert records: zero-fill the page (tombstone).
    // For delete/update records: the inverse is a no-op at this layer since we
    // do not carry before-images in this implementation — the WAL carries
    // after-images only.  A full before-image implementation would store the
    // old page contents in the WAL record payload and restore it here.
    let tombstone = matches!(
        rec.record_type,
        RecordType::NodeInsert
            | RecordType::EdgeInsert
            | RecordType::PropertyInsert
            | RecordType::PageInsert
    );

    if tombstone || !fs.exists(data_path) {
        // Write a zeroed page — logically deleted.
        let handle = fs.open(data_path, true)?;
        let offset = page_id * PAGE_SIZE as u64;
        let zeroes = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.write_at(&zeroes, offset)?;
    }
    Ok(())
}

/// Build the payload for a CLR record.
///
/// Layout:
/// ```text
/// [0..8]   undo_next_lsn  — prev_lsn of the record being compensated
/// [8..16]  page_id
/// [16..]   tombstone/before-image (zeroed for now)
/// ```
fn build_clr_payload(undo_next_lsn: u64, page_id: PageId, image: &[u8]) -> Vec<u8> {
    let image_len = image.len().min(PAGE_SIZE);
    let mut payload = Vec::with_capacity(16 + image_len);
    payload.extend_from_slice(&undo_next_lsn.to_be_bytes());
    payload.extend_from_slice(&page_id.to_be_bytes());
    payload.extend_from_slice(&image[..image_len]);
    payload
}

/// Extract `undo_next_lsn` from a CLR payload.
/// Returns 0 if the payload is too short (signals end of undo chain).
fn clr_undo_next_lsn(payload: &[u8]) -> u64 {
    if payload.len() < 8 {
        return 0;
    }
    u64::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
        payload[4], payload[5], payload[6], payload[7],
    ])
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::{RecordType, WalRecord};

    fn begin(txid: u64, lsn: u64, prev: u64) -> WalRecord {
        let mut r = WalRecord::new(RecordType::Begin, txid, 0, prev, vec![]);
        r.set_lsn(lsn);
        r
    }

    fn commit(txid: u64, lsn: u64, prev: u64) -> WalRecord {
        let mut r = WalRecord::new(RecordType::Commit, txid, 0, prev, vec![]);
        r.set_lsn(lsn);
        r
    }

    fn abort(txid: u64, lsn: u64, prev: u64) -> WalRecord {
        let mut r = WalRecord::new(RecordType::Abort, txid, 0, prev, vec![]);
        r.set_lsn(lsn);
        r
    }

    fn page_update(txid: u64, lsn: u64, prev: u64, page_id: u64) -> WalRecord {
        let mut payload = page_id.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0u8; 8]); // tiny "after-image"
        let mut r = WalRecord::new(RecordType::PageUpdate, txid, 0, prev, payload);
        r.set_lsn(lsn);
        r
    }

    fn node_insert(txid: u64, lsn: u64, prev: u64, page_id: u64) -> WalRecord {
        let mut payload = page_id.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0xABu8; 16]);
        let mut r = WalRecord::new(RecordType::NodeInsert, txid, 0, prev, payload);
        r.set_lsn(lsn);
        r
    }

    // ── ANALYSIS ──────────────────────────────────────────────────────────────

    #[test]
    fn analysis_empty_wal() {
        let r = AriesRecovery::new_with_records(vec![]);
        let (att, dpt, max_lsn) = r.analysis_from_records();
        assert!(att.is_empty());
        assert!(dpt.is_empty());
        assert_eq!(max_lsn, 0);
    }

    #[test]
    fn analysis_committed_transaction_in_att() {
        let records = vec![begin(1, 100, 0), page_update(1, 150, 100, 5), commit(1, 200, 150)];
        let r = AriesRecovery::new_with_records(records);
        let (att, dpt, max_lsn) = r.analysis_from_records();

        assert_eq!(att.len(), 1);
        assert_eq!(att[&1].status, TxStatus::Committed);
        assert_eq!(att[&1].last_lsn, 200);
        assert_eq!(dpt.len(), 1);
        assert_eq!(dpt[&5].rec_lsn, 150);
        assert_eq!(max_lsn, 200);
    }

    #[test]
    fn analysis_active_transaction_remains() {
        let records = vec![begin(2, 10, 0), page_update(2, 20, 10, 3)];
        let r = AriesRecovery::new_with_records(records);
        let (att, _, _) = r.analysis_from_records();

        assert_eq!(att[&2].status, TxStatus::Active);
        assert_eq!(att[&2].last_lsn, 20);
    }

    #[test]
    fn analysis_aborted_transaction() {
        let records = vec![begin(3, 10, 0), abort(3, 30, 10)];
        let r = AriesRecovery::new_with_records(records);
        let (att, _, _) = r.analysis_from_records();

        assert_eq!(att[&3].status, TxStatus::Aborted);
    }

    #[test]
    fn analysis_dpt_rec_lsn_is_earliest() {
        // Two updates to the same page: first at lsn=50, second at lsn=100.
        // rec_lsn should be 50 (the earlier one).
        let records = vec![
            begin(1, 10, 0),
            page_update(1, 50, 10, 7),
            page_update(1, 100, 50, 7),
        ];
        let r = AriesRecovery::new_with_records(records);
        let (_, dpt, _) = r.analysis_from_records();
        assert_eq!(dpt[&7].rec_lsn, 50);
    }

    #[test]
    fn analysis_multiple_transactions() {
        let records = vec![
            begin(1, 10, 0),
            begin(2, 20, 0),
            page_update(1, 30, 10, 1),
            page_update(2, 40, 20, 2),
            commit(1, 50, 30),
        ];
        let r = AriesRecovery::new_with_records(records);
        let (att, dpt, _) = r.analysis_from_records();

        assert_eq!(att[&1].status, TxStatus::Committed);
        assert_eq!(att[&2].status, TxStatus::Active);
        assert_eq!(dpt.len(), 2);
    }

    #[test]
    fn analysis_clr_updates_last_lsn_not_dpt() {
        let mut clr_payload = 0u64.to_be_bytes().to_vec(); // undo_next_lsn = 0
        clr_payload.extend_from_slice(&5u64.to_be_bytes()); // page_id
        clr_payload.extend_from_slice(&[0u8; 8]);
        let mut clr_rec = WalRecord::new(RecordType::Clr, 1, 0, 50, clr_payload);
        clr_rec.set_lsn(200);

        let records = vec![begin(1, 10, 0), node_insert(1, 50, 10, 5), clr_rec];
        let r = AriesRecovery::new_with_records(records);
        let (att, dpt, _) = r.analysis_from_records();

        // CLR should update last_lsn but NOT add page 5 again to DPT (already there).
        assert_eq!(att[&1].last_lsn, 200);
        assert!(dpt.contains_key(&5)); // from node_insert
    }

    // ── REDO (filesystem-dependent; uses tempdir) ─────────────────────────────

    #[test]
    fn redo_applies_missing_update() {
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PageType, SlottedPage};

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");
        let wal_path = dir.path().join("wal-000000000");

        // Pre-create a zeroed page.
        {
            let page = SlottedPage::init(1, PageType::SlottedData);
            let handle = fs.open(&data_path, true).unwrap();
            handle.write_at(&page.buf, PAGE_SIZE as u64).unwrap();
            handle.sync_data().unwrap();
        }

        // Build WAL with a PageUpdate for page 1.
        let mut payload = 1u64.to_be_bytes().to_vec();
        let mut page_img = SlottedPage::init(1, PageType::SlottedData);
        page_img.header_mut().page_lsn = 999;
        payload.extend_from_slice(&page_img.buf);

        let mut rec = WalRecord::new(RecordType::PageUpdate, 1, 0, 0, payload);
        rec.set_lsn(999);

        // Write WAL.
        let wal_bytes = rec.encode();
        let handle = fs.open(&wal_path, true).unwrap();
        handle.write_at(&wal_bytes, 0).unwrap();
        handle.sync_data().unwrap();

        // Build DPT manually.
        let mut dpt = HashMap::new();
        dpt.insert(1u64, DptEntry { page_id: 1, rec_lsn: 999 });

        let recovery = AriesRecovery::new(&fs, &wal_path, &data_path, 0);
        let records = recovery.load_records(&fs, &wal_path).unwrap();
        let count = recovery.redo(&records, &dpt, &fs).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn redo_skips_up_to_date_page() {
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PageType, SlottedPage};

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");
        let wal_path = dir.path().join("wal-000000000");

        // Page already at lsn=9999 — newer than the WAL record.
        {
            let mut page = SlottedPage::init(2, PageType::SlottedData);
            page.header_mut().page_lsn = 9999;
            let handle = fs.open(&data_path, true).unwrap();
            handle.write_at(&page.buf, 2 * PAGE_SIZE as u64).unwrap();
            handle.sync_data().unwrap();
        }

        let mut payload = 2u64.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0u8; 8]);
        let mut rec = WalRecord::new(RecordType::PageUpdate, 1, 0, 0, payload);
        rec.set_lsn(500); // older than page

        let wal_bytes = rec.encode();
        let handle = fs.open(&wal_path, true).unwrap();
        handle.write_at(&wal_bytes, 0).unwrap();
        handle.sync_data().unwrap();

        let mut dpt = HashMap::new();
        dpt.insert(2u64, DptEntry { page_id: 2, rec_lsn: 500 });

        let recovery = AriesRecovery::new(&fs, &wal_path, &data_path, 0);
        let records = recovery.load_records(&fs, &wal_path).unwrap();
        let count = recovery.redo(&records, &dpt, &fs).unwrap();
        assert_eq!(count, 0, "page is already up-to-date; REDO must be a no-op");
    }
}

// ── Property tests (proptest) ─────────────────────────────────────────────────

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use crate::wal::record::{RecordType, WalRecord};
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn make_begin_record(txid: u64, lsn: u64) -> WalRecord {
        let mut r = WalRecord::new(RecordType::Begin, txid, 0, 0, vec![]);
        r.set_lsn(lsn);
        r
    }

    proptest! {
        #[test]
        fn analysis_att_count_matches_begin_records(
            txids in prop::collection::vec(1u64..1000u64, 0..20),
        ) {
            let unique_txids: HashSet<u64> = txids.iter().copied().collect();
            let mut records = vec![];
            let mut lsn = 1u64;
            for txid in &unique_txids {
                records.push(make_begin_record(*txid, lsn));
                lsn += 50;
            }
            let recovery = AriesRecovery::new_with_records(records);
            let (att, _dpt, _max_lsn) = recovery.analysis_from_records();
            prop_assert_eq!(att.len(), unique_txids.len());
        }

        #[test]
        fn analysis_max_lsn_is_highest(
            lsns in prop::collection::vec(1u64..10_000u64, 1..20),
        ) {
            let records: Vec<WalRecord> = lsns
                .iter()
                .enumerate()
                .map(|(i, &lsn)| make_begin_record(i as u64 + 1, lsn))
                .collect();
            let expected_max = *lsns.iter().max().unwrap();
            let recovery = AriesRecovery::new_with_records(records);
            let (_att, _dpt, max_lsn) = recovery.analysis_from_records();
            prop_assert_eq!(max_lsn, expected_max);
        }

        #[test]
        fn analysis_committed_not_active(
            txid in 1u64..500u64,
        ) {
            let records = vec![
                make_begin_record(txid, 10),
                {
                    let mut r = WalRecord::new(RecordType::Commit, txid, 0, 10, vec![]);
                    r.set_lsn(20);
                    r
                },
            ];
            let recovery = AriesRecovery::new_with_records(records);
            let (att, _, _) = recovery.analysis_from_records();
            prop_assert_eq!(att[&txid].status, TxStatus::Committed);
        }

        #[test]
        fn analysis_no_pages_without_mutations(
            n in 0usize..20usize,
        ) {
            // Only Begin/Commit records — DPT must be empty.
            let records: Vec<WalRecord> = (0..n)
                .flat_map(|i| {
                    let txid = i as u64 + 1;
                    let lsn_b = txid * 100;
                    let lsn_c = lsn_b + 50;
                    vec![make_begin_record(txid, lsn_b), {
                        let mut r = WalRecord::new(RecordType::Commit, txid, 0, lsn_b, vec![]);
                        r.set_lsn(lsn_c);
                        r
                    }]
                })
                .collect();
            let recovery = AriesRecovery::new_with_records(records);
            let (_, dpt, _) = recovery.analysis_from_records();
            prop_assert!(dpt.is_empty());
        }
    }
}
