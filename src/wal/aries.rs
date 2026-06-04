//! Full three-phase ARIES crash recovery (Tasks 106, 107, 108, 148, 149, 150).
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
//!    When a `CheckpointEnd` record is found during ANALYSIS, the DPT is seeded
//!    from the checkpoint payload, reducing the amount of work REDO must do.
//!
//! 2. **REDO** — replay after-images for all pages in the DPT, starting from
//!    `min(rec_lsn)`.  Each application is idempotent: a page whose
//!    `page_lsn >= record.lsn` is skipped.
//!
//! 3. **UNDO** — for every `Active` or `Aborted` transaction remaining in the
//!    ATT, traverse backward via `prev_lsn`, apply the **before-image**
//!    embedded in the WAL record payload (Task 149), and emit a Compensation
//!    Log Record (CLR) with `undo_next_lsn = rec.prev_lsn` so that a crash
//!    during UNDO converges correctly on the next restart.
//!
//! # Multi-segment recovery (Task 150)
//!
//! [`AriesRecovery::new`] accepts a WAL *directory*.  On startup it enumerates
//! all segment files (`wal-NNNNNNNNN`) in ascending order and reads them in
//! sequence, building one unified record stream.  LSNs are
//! `(segment_id << 32) | intra_segment_offset`; the comparisons inside ANALYSIS
//! and REDO use these opaque 64-bit values directly.
//!
//! # Testability
//!
//! [`AriesRecovery::new_with_records`] accepts an in-memory record slice so
//! that property tests can drive all three phases without touching the
//! filesystem.

use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::page::{PAGE_SIZE, PageId};
use crate::wal::record::{RecordType, WalRecord};
use crate::wal::writer::WalWriter;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

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
    /// Directory containing WAL segment files (`wal-NNNNNNNNN`).
    wal_dir: Option<PathBuf>,
    data_path: Option<&'a Path>,
    /// WAL LSN from which ANALYSIS starts (typically the checkpoint begin LSN).
    checkpoint_lsn: u64,
    /// Pre-loaded in-memory records used when `fs` is `None` (test mode).
    preloaded_records: Option<Vec<WalRecord>>,
}

impl<'a> AriesRecovery<'a> {
    /// Production constructor: reads all WAL segments from `wal_dir`.
    ///
    /// `wal_dir` is the directory that contains the `wal-NNNNNNNNN` segment
    /// files.  Recovery reads all segments in ascending order.
    ///
    /// `checkpoint_lsn` is the LSN stored in the superblock; ANALYSIS begins
    /// from this point so that only the records since the last fuzzy checkpoint
    /// need to be replayed.
    pub fn new(
        fs: &'a dyn FileSystem,
        wal_dir: &Path,
        data_path: &'a Path,
        checkpoint_lsn: u64,
    ) -> Self {
        Self {
            fs: Some(fs),
            wal_dir: Some(wal_dir.to_path_buf()),
            data_path: Some(data_path),
            checkpoint_lsn,
            preloaded_records: None,
        }
    }

    /// Test constructor: uses an in-memory record slice, no filesystem I/O.
    ///
    /// `analysis_from_records` and the full `recover_from_slice` path become
    /// usable, but `recover` (which requires a real filesystem) will return an
    /// error.
    pub fn new_with_records(records: Vec<WalRecord>) -> Self {
        Self {
            fs: None,
            wal_dir: None,
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
    /// Reads all WAL segments in the configured `wal_dir` in ascending order
    /// to build a unified record stream, then executes ANALYSIS, REDO, and
    /// UNDO in sequence.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if reading the WAL or writing to data pages
    /// fails.
    pub fn recover(&self, wal: &mut WalWriter) -> io::Result<RecoveryResult> {
        let fs = self.fs.expect("recover requires a filesystem (use new())");
        let wal_dir = self
            .wal_dir
            .as_deref()
            .expect("recover requires a WAL directory (use new())");

        let records = self.load_all_segments(fs, wal_dir)?;
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

    // ── Internal: load records from all WAL segments ─────────────────────────

    /// Read all WAL segments in `wal_dir` in ascending order and return a
    /// unified, flat record vector.
    ///
    /// Segments are files matching `wal-NNNNNNNNN`.  Decoding stops at the
    /// first corrupt record within each segment (truncated tail handling).
    ///
    /// Each record's LSN is the value stored in the on-disk encoding, which
    /// uses the `(segment_id << 32) | intra_segment_offset` scheme (Task 150).
    pub(crate) fn load_all_segments(
        &self,
        fs: &dyn FileSystem,
        wal_dir: &Path,
    ) -> io::Result<Vec<WalRecord>> {
        let mut all_records = Vec::new();
        let archive_dir = wal_dir.join("wal-archive");

        // Records below the checkpoint are already durably applied (and may have
        // been archived), so start scanning at the checkpoint's segment.
        let start_seg = self.checkpoint_lsn >> 32;
        let highest = Self::highest_segment_id(fs, wal_dir);

        for seg_id in start_seg..=highest {
            let live = wal_dir.join(format!("wal-{:09}", seg_id));
            // A still-needed segment (above the checkpoint) may have been moved to
            // the archive by retention-based archiving, so consult `wal-archive/`
            // when it is missing from the live directory.  Do NOT stop at the gap
            // between the archive and the live window — later live segments must
            // still be read (M23).
            let path = if fs.exists(&live) {
                live
            } else {
                let archived = archive_dir.join(format!("wal-{:09}", seg_id));
                if fs.exists(&archived) {
                    archived
                } else {
                    continue;
                }
            };
            let mut seg_records = self.load_single_segment(fs, &path, seg_id as u32)?;
            all_records.append(&mut seg_records);
        }

        Ok(all_records)
    }

    /// Highest segment id present across the live WAL directory AND the archive.
    /// The archive holds a contiguous low range `[0, cutoff]` and the live
    /// directory the contiguous high range `[cutoff+1, current]`, so the union is
    /// contiguous and a simple scan finds the true maximum (M23).
    fn highest_segment_id(fs: &dyn FileSystem, wal_dir: &Path) -> u64 {
        let archive_dir = wal_dir.join("wal-archive");
        let mut max_id = 0u64;
        for id in 0u64..=u32::MAX as u64 {
            let live = wal_dir.join(format!("wal-{:09}", id));
            let archived = archive_dir.join(format!("wal-{:09}", id));
            if fs.exists(&live) || fs.exists(&archived) {
                max_id = id;
            } else if id > max_id {
                break;
            }
        }
        max_id
    }

    /// Read one WAL segment file and decode all valid records.
    ///
    /// Records whose stored LSN does not match the expected segment are
    /// accepted as-is (the LSN is authoritative from the on-disk encoding).
    fn load_single_segment(
        &self,
        fs: &dyn FileSystem,
        seg_path: &Path,
        _seg_id: u32,
    ) -> io::Result<Vec<WalRecord>> {
        let handle = fs.open(seg_path, false)?;
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
                None => break, // truncated / corrupt tail — stop here
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
            &analysis_records
                .iter()
                .map(|r| (*r).clone())
                .collect::<Vec<_>>(),
        );

        let redo_count = if let Some(fs) = self.fs {
            self.redo(records, &att, &dpt, fs)?
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

                // ── Physical page records: update ATT + DPT ──────────────────
                //
                // Only records whose payloads carry a real page image are tracked
                // in the DPT.  Logical entity records (NodeInsert etc.) use
                // entity-ids — not page-ids — in their first 8 payload bytes and
                // therefore must NOT be added to the DPT; doing so would cause
                // REDO to write garbage to the wrong offsets in the data file.
                RecordType::PageInsert
                | RecordType::PageUpdate
                | RecordType::PageFree
                | RecordType::BitmapUpdate
                | RecordType::IndexPageInsert
                | RecordType::IndexPageUpdate
                | RecordType::IndexPageFree => {
                    // Payload: [8-byte page_id][page image bytes...].
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

                // ── Logical entity records: update ATT only ───────────────────
                //
                // These records carry entity-level data (node/edge/property ids
                // and record bytes), not raw page images.  They update the ATT
                // but do NOT contribute to the DPT — REDO is handled at the
                // page level by `PageUpdate`/`PageInsert` records (when the
                // engine is extended to emit those), or implicitly through
                // `rebuild_indexes` on open.
                RecordType::NodeInsert
                | RecordType::NodeDelete
                | RecordType::NodeUpdate
                | RecordType::EdgeInsert
                | RecordType::EdgeDelete
                | RecordType::EdgeUpdate
                | RecordType::PropertyInsert
                | RecordType::PropertyUpdate
                | RecordType::RdfTripleInsert
                | RecordType::RdfTripleDelete => {
                    // Update ATT last_lsn for this txid.
                    if let Some(entry) = att.get_mut(&rec.txid)
                        && lsn > entry.last_lsn
                    {
                        entry.last_lsn = lsn;
                    }
                }

                // ── Checkpoint end: seed DPT from the checkpoint record ────
                RecordType::CheckpointEnd => {
                    // Payload format written by Checkpoint::run():
                    //   [0..4]   dirty_page_count: u32
                    //   [4..4+N*16] N * (page_id: u64, rec_lsn: u64)
                    //   [4+N*16..] active_tx_count: u32 (followed by tx entries)
                    seed_dpt_from_checkpoint(&rec.payload, &mut dpt);
                }

                // ── Checkpoint begin / segment descriptor / compaction:
                //    purely logical markers with no page side effects ──────────
                RecordType::CheckpointBegin
                | RecordType::SegmentDescriptor
                | RecordType::CompactionBegin
                | RecordType::CompactionEnd => {}
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
        att: &HashMap<u64, AttEntry>,
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

            // REDO is only applicable to **physical** page-image records whose
            // payloads carry a full or partial page image starting at offset 0.
            //
            // Logical entity records (NodeInsert, EdgeInsert, etc.) written by
            // the storage engine encode entity identifiers and record bytes, not
            // raw page images.  Applying them here would write garbage to the
            // data file.  The engine rebuilds entity-level state from pages
            // during `open()` → `rebuild_indexes()`, so logical records do not
            // need physical REDO.
            //
            // CLRs written during UNDO carry the before-image (i.e. the page
            // state after the inverse operation) and DO need REDO.
            let is_physical_page_record = matches!(
                rec.record_type,
                RecordType::PageInsert
                    | RecordType::PageUpdate
                    | RecordType::PageFree
                    | RecordType::BitmapUpdate
                    | RecordType::IndexPageInsert
                    | RecordType::IndexPageUpdate
                    | RecordType::IndexPageFree
                    | RecordType::Clr
            );
            if !is_physical_page_record {
                continue;
            }

            // Option A (no-steal / no-force): only replay records of COMMITTED
            // transactions. A loser never wrote its data page — data writes
            // happen only after the commit record is durable — so replaying its
            // records would resurrect uncommitted data. CLRs are always replayed
            // (they are completed undo work). This is sound while ANALYSIS scans
            // from LSN 0 so every committed txn is in the ATT; a future fuzzy
            // checkpoint (H7) must flush dirty pages to preserve the invariant.
            if rec.record_type != RecordType::Clr
                && att.get(&rec.txid).map(|e| e.status) != Some(TxStatus::Committed)
            {
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
        let lsn_index: HashMap<u64, &WalRecord> = records.iter().map(|r| (r.lsn, r)).collect();

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

                // Only **physical** page-mutation records produce inverse ops.
                //
                // Logical entity records (NodeInsert/EdgeInsert/PropertyInsert
                // and their *Update/*Delete variants) encode an *entity id* — not
                // a page id — in their first 8 payload bytes and carry no
                // before-image.  Routing them through `apply_inverse` would read
                // the entity id as a page id and zero-fill the page at
                // `entity_id * PAGE_SIZE`, corrupting unrelated metadata/data
                // pages (node_id 1 → mirror superblock, 2 → bitmap, N → data page
                // N).  They are therefore excluded from physical UNDO — exactly as
                // they are excluded from the DPT in ANALYSIS and from replay in
                // REDO — and must be undone logically.  See reliability-audit
                // finding C2 (2026-06-04).
                let undoable = matches!(
                    rec.record_type,
                    RecordType::PageInsert
                        | RecordType::PageUpdate
                        | RecordType::PageFree
                        | RecordType::BitmapUpdate
                        | RecordType::IndexPageInsert
                        | RecordType::IndexPageUpdate
                        | RecordType::IndexPageFree
                );

                if undoable && let Some((page_id, after_image)) = extract_page_and_image(rec) {
                    // Determine what the page will look like after the inverse.
                    // If the WAL record carries a before-image, that is the state
                    // we restore; otherwise (for inserts) it is a zero-filled page.
                    let clr_image: Vec<u8> = if let Some(bi) = extract_before_image(&rec.payload) {
                        bi.to_vec()
                    } else {
                        // Insert tombstone: zeroed page.
                        vec![0u8; PAGE_SIZE]
                    };

                    // Apply inverse operation on the page.
                    apply_inverse(fs, data_path, rec, page_id, after_image)?;

                    // Write CLR to WAL.
                    // `undo_next_lsn` = rec.prev_lsn so that if recovery crashes
                    // during UNDO it can skip the already-compensated record and
                    // continue from the next one in the chain (idempotency).
                    let clr_payload = build_clr_payload(rec.prev_lsn, page_id, &clr_image);
                    let clr = WalRecord::new(
                        RecordType::Clr,
                        rec.txid,
                        0, // LSN assigned by WalWriter
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
        payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
        payload[7],
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

    // Recompute the page checksum so the recovered page is self-consistent:
    // stamping page_lsn above invalidated the checksum that was computed when
    // the after-image was captured.  Only do this for full-page after-images
    // that already carry a valid page magic, so partial/synthetic images used
    // by lower-level tests are left untouched.
    if copy_len == PAGE_SIZE
        && crate::storage::page::SlottedPage::has_valid_magic_bytes(&page_buf)
    {
        crate::storage::page::SlottedPage::update_checksum_bytes(&mut page_buf);
    }

    handle.write_at(&page_buf, offset)?;
    Ok(())
}

/// Apply the inverse of `rec` to bring `page_id` back to its pre-operation state.
///
/// # Before-image extraction (Task 149)
///
/// The WAL payload may carry a before-image appended after the after-image
/// content, framed by a trailing CRC, length and magic sentinel `BIMG`
/// (`0x42494D47`) so the section can be located and validated unambiguously:
///
/// ```text
/// [original payload][before_image bytes][crc32c(image) 4][before_image_len 4][0x42494D47 magic 4]
/// ```
///
/// When a before-image is present it is written to `page_id` to restore the
/// page to its pre-mutation state.  When absent (e.g. for `NodeInsert` where
/// there is no prior state), a zero-filled tombstone is written instead.
fn apply_inverse(
    fs: &dyn FileSystem,
    data_path: &Path,
    rec: &WalRecord,
    page_id: PageId,
    _after_image: &[u8],
) -> io::Result<()> {
    // Try to extract a before-image from the WAL record payload.
    if let Some(before_image) = extract_before_image(&rec.payload) {
        // Restore the before-image to the page.
        let handle = fs.open(data_path, true)?;
        let offset = page_id * PAGE_SIZE as u64;
        let copy_len = before_image.len().min(PAGE_SIZE);
        let mut page_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        page_buf[..copy_len].copy_from_slice(&before_image[..copy_len]);
        handle.write_at(&page_buf, offset)?;
        return Ok(());
    }

    // No before-image: fall back to tombstone strategy for insert records, and
    // no-op for other record types (the state was already at the after-image).
    //
    // Only **physical** insert records reach this point — logical entity inserts
    // are excluded from physical UNDO in `undo` (finding C2), so reading their
    // entity id as a page id can never zero an unrelated page here.
    let is_insert = matches!(
        rec.record_type,
        RecordType::PageInsert | RecordType::IndexPageInsert
    );

    if is_insert {
        // Write a zeroed page — logically deletes the inserted entity.
        let handle = fs.open(data_path, true)?;
        let offset = page_id * PAGE_SIZE as u64;
        let zeroes = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.write_at(&zeroes, offset)?;
    }
    // For update/delete without a before-image: we cannot restore the prior
    // state.  This is acceptable for records written by an older version of
    // the engine that did not embed before-images.
    Ok(())
}

/// Magic sentinel used to identify the before-image section in a WAL payload.
///
/// A WAL record payload that carries a before-image has the following structure:
///
/// ```text
/// [original after-image content][before_image bytes][crc32c 4 bytes][before_image_len 4 bytes][BEFORE_IMAGE_MAGIC 4 bytes]
/// ```
const BEFORE_IMAGE_MAGIC: u32 = 0x42494D47; // "BIMG"

/// Append a before-image to an existing WAL payload.
///
/// The caller should invoke this *after* constructing the standard after-image
/// payload and *before* handing the payload to [`WalRecord::new`].
///
/// # Arguments
///
/// * `payload` — the existing after-image payload bytes; mutated in place.
/// * `before_image` — the raw page bytes before the mutation.
pub fn embed_before_image(payload: &mut Vec<u8>, before_image: &[u8]) {
    let trimmed_len = before_image.len().min(PAGE_SIZE);
    let image = &before_image[..trimmed_len];
    // Trailer: [image][crc32c(image) u32][image_len u32][MAGIC u32].
    // The magic occupies the FINAL 4 bytes so extraction is deterministic (no
    // backward scan), and the explicit length + CRC make a coincidental match of
    // arbitrary page-image bytes effectively impossible (finding M25).
    payload.extend_from_slice(image);
    payload.extend_from_slice(&crc32c::crc32c(image).to_be_bytes());
    payload.extend_from_slice(&(trimmed_len as u32).to_be_bytes());
    payload.extend_from_slice(&BEFORE_IMAGE_MAGIC.to_be_bytes());
}

/// Extract the before-image from a WAL payload if the sentinel is present.
///
/// Returns `None` if no before-image was embedded.
fn extract_before_image(payload: &[u8]) -> Option<&[u8]> {
    // Trailer layout written by `embed_before_image`:
    //   [before_image bytes][crc32c(image) u32][image_len u32][MAGIC u32]
    // The magic is the final 4 bytes, so extraction is deterministic (no
    // backward scan) and is validated by an explicit length and CRC.  This makes
    // a coincidental match of arbitrary page-image bytes — e.g. a `PageUpdate`
    // after-image that happens to contain the magic — effectively impossible
    // (finding M25): a false positive would require the final 4 bytes to equal
    // the magic AND the preceding length to be self-consistent AND the CRC over
    // the implied image to match.
    let n = payload.len();
    if n < 12 {
        return None;
    }
    let magic = u32::from_be_bytes(payload[n - 4..n].try_into().ok()?);
    if magic != BEFORE_IMAGE_MAGIC {
        return None;
    }
    let image_len = u32::from_be_bytes(payload[n - 8..n - 4].try_into().ok()?) as usize;
    // 12 = crc(4) + len(4) + magic(4); the image must fit before the trailer.
    if image_len > PAGE_SIZE || image_len + 12 > n {
        return None;
    }
    let crc_stored = u32::from_be_bytes(payload[n - 12..n - 8].try_into().ok()?);
    let image = &payload[n - 12 - image_len..n - 12];
    if crc32c::crc32c(image) != crc_stored {
        return None;
    }
    Some(image)
}

/// Build the payload for a CLR record.
///
/// Layout:
/// ```text
/// [0..8]   undo_next_lsn  — prev_lsn of the record being compensated
/// [8..16]  page_id
/// [16..]   before-image bytes (the state written by the inverse operation)
/// ```
fn build_clr_payload(undo_next_lsn: u64, page_id: PageId, before_image: &[u8]) -> Vec<u8> {
    let image_len = before_image.len().min(PAGE_SIZE);
    let mut payload = Vec::with_capacity(16 + image_len);
    payload.extend_from_slice(&undo_next_lsn.to_be_bytes());
    payload.extend_from_slice(&page_id.to_be_bytes());
    payload.extend_from_slice(&before_image[..image_len]);
    payload
}

/// Extract `undo_next_lsn` from a CLR payload.
/// Returns 0 if the payload is too short (signals end of undo chain).
fn clr_undo_next_lsn(payload: &[u8]) -> u64 {
    if payload.len() < 8 {
        return 0;
    }
    u64::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
        payload[7],
    ])
}

/// Seed the Dirty Page Table from a `CheckpointEnd` WAL record payload.
///
/// Payload format (written by [`Checkpoint::run`]):
/// ```text
/// [0..4]        dirty_page_count: u32 big-endian
/// [4..4+N*16]   N entries of (page_id: u64 big-endian, rec_lsn: u64 big-endian)
/// [4+N*16..]    active_tx_count: u32 (ignored; placeholder for future use)
/// ```
///
/// Existing DPT entries are preserved (oldest `rec_lsn` wins); only pages not
/// already in the DPT are added from the checkpoint record.
fn seed_dpt_from_checkpoint(payload: &[u8], dpt: &mut HashMap<u64, DptEntry>) {
    if payload.len() < 4 {
        return;
    }
    let count = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    // The `count` field comes from a CheckpointEnd record that passed only the
    // record-level CRC; a corrupt/torn record could claim an arbitrary value.
    // Compute the required length with checked arithmetic (no overflow even on
    // 32-bit) and bail on any inconsistency rather than trusting the field or
    // panicking on the recovery path (finding L8).
    let Some(required_len) = count.checked_mul(16).and_then(|n| n.checked_add(4)) else {
        return; // count * 16 + 4 overflows usize → impossible record; ignore
    };
    if payload.len() < required_len {
        return; // truncated/inconsistent checkpoint record; ignore
    }
    for i in 0..count {
        let base = 4 + i * 16;
        // The slices are guaranteed to be 8 bytes by the length check above, but
        // read them totally (no `expect`) so the recovery path can never panic.
        let (Ok(page_bytes), Ok(lsn_bytes)) = (
            <[u8; 8]>::try_from(&payload[base..base + 8]),
            <[u8; 8]>::try_from(&payload[base + 8..base + 16]),
        ) else {
            return;
        };
        let page_id = u64::from_be_bytes(page_bytes);
        let rec_lsn = u64::from_be_bytes(lsn_bytes);
        // Only insert if not already present; oldest rec_lsn wins.
        dpt.entry(page_id).or_insert(DptEntry { page_id, rec_lsn });
    }
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
        let records = vec![
            begin(1, 100, 0),
            page_update(1, 150, 100, 5),
            commit(1, 200, 150),
        ];
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
        // Use a PageInsert (physical record) so it appears in the DPT.
        // Then add a CLR that compensates for it.
        // The CLR should update last_lsn but NOT add page 5 again to the DPT.
        let mut page_payload = 5u64.to_be_bytes().to_vec();
        page_payload.extend_from_slice(&[0u8; 8]); // tiny after-image
        let mut page_insert_rec = WalRecord::new(RecordType::PageInsert, 1, 0, 10, page_payload);
        page_insert_rec.set_lsn(50);

        let mut clr_payload = 0u64.to_be_bytes().to_vec(); // undo_next_lsn = 0
        clr_payload.extend_from_slice(&5u64.to_be_bytes()); // page_id
        clr_payload.extend_from_slice(&[0u8; 8]);
        let mut clr_rec = WalRecord::new(RecordType::Clr, 1, 0, 50, clr_payload);
        clr_rec.set_lsn(200);

        let records = vec![begin(1, 10, 0), page_insert_rec, clr_rec];
        let r = AriesRecovery::new_with_records(records);
        let (att, dpt, _) = r.analysis_from_records();

        // CLR should update last_lsn but NOT add page 5 again to DPT (already there from PageInsert).
        assert_eq!(att[&1].last_lsn, 200);
        assert!(
            dpt.contains_key(&5),
            "page 5 should be in DPT from PageInsert record"
        );
    }

    // ── REDO (filesystem-dependent; uses tempdir) ─────────────────────────────

    #[test]
    fn redo_applies_missing_update() {
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PageType, SlottedPage};

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");
        // WAL directory — create segment 0 inside it.
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let wal_path = wal_dir.join("wal-000000000");

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
        dpt.insert(
            1u64,
            DptEntry {
                page_id: 1,
                rec_lsn: 999,
            },
        );

        let recovery = AriesRecovery::new(&fs, &wal_dir, &data_path, 0);
        let records = recovery.load_all_segments(&fs, &wal_dir).unwrap();
        // REDO replays committed records only (Option A): mark txid 1 committed.
        let mut att = HashMap::new();
        att.insert(
            1u64,
            AttEntry {
                txid: 1,
                status: TxStatus::Committed,
                last_lsn: 999,
            },
        );
        let count = recovery.redo(&records, &att, &dpt, &fs).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn redo_skips_up_to_date_page() {
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PageType, SlottedPage};

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let wal_path = wal_dir.join("wal-000000000");

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
        dpt.insert(
            2u64,
            DptEntry {
                page_id: 2,
                rec_lsn: 500,
            },
        );

        let recovery = AriesRecovery::new(&fs, &wal_dir, &data_path, 0);
        let records = recovery.load_all_segments(&fs, &wal_dir).unwrap();
        // Committed txn so the record reaches the idempotency check (not skipped
        // by the commit-filter); it must still be a no-op because the page is newer.
        let mut att = HashMap::new();
        att.insert(
            1u64,
            AttEntry {
                txid: 1,
                status: TxStatus::Committed,
                last_lsn: 500,
            },
        );
        let count = recovery.redo(&records, &att, &dpt, &fs).unwrap();
        assert_eq!(count, 0, "page is already up-to-date; REDO must be a no-op");
    }

    #[test]
    fn load_all_segments_reads_archived_and_live_across_the_gap() {
        // Regression gate for finding M23 (2026-06-04): recovery must not stop at
        // the gap left by retention-based archiving, and must consult
        // `wal-archive/` for still-needed segments above the checkpoint.
        use crate::io::posix::PosixFileSystem;

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let wal_dir = dir.path().join("wal");
        let archive_dir = wal_dir.join("wal-archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        let data_path = dir.path().join("data.db");

        let write_seg = |path: &std::path::Path, txid: u64| {
            let bytes = WalRecord::new(RecordType::Begin, txid, 0, 0, vec![]).encode();
            let handle = fs.open(path, true).unwrap();
            handle.write_at(&bytes, 0).unwrap();
            handle.sync_data().unwrap();
        };

        // Segments 0,1 archived (contiguous low range); 2,3 live (contiguous high
        // range).  The live directory therefore has a "gap" at ids 0 and 1.
        write_seg(&archive_dir.join("wal-000000000"), 10);
        write_seg(&archive_dir.join("wal-000000001"), 11);
        write_seg(&wal_dir.join("wal-000000002"), 12);
        write_seg(&wal_dir.join("wal-000000003"), 13);

        // Checkpoint into segment 1: recovery must read segments 1 (from the
        // archive), 2 and 3 (live), but not segment 0 (below the checkpoint).
        let checkpoint_lsn = (1u64 << 32) | 1;
        let recovery = AriesRecovery::new(&fs, &wal_dir, &data_path, checkpoint_lsn);
        let records = recovery.load_all_segments(&fs, &wal_dir).unwrap();
        let txids: Vec<u64> = records.iter().map(|r| r.txid).collect();

        assert!(txids.contains(&11), "must read archived segment 1 above the checkpoint");
        assert!(txids.contains(&12), "must read live segment 2 past the archiving gap");
        assert!(txids.contains(&13), "must read live segment 3");
        assert!(!txids.contains(&10), "must not read segment 0 (below the checkpoint)");
    }

    #[test]
    fn redo_reconstructs_committed_page_and_skips_loser() {
        // Regression gate for findings C1/C3 (Option A no-steal/no-force,
        // 2026-06-04): a committed PageInsert whose data-page write was lost is
        // reconstructed by REDO, while an uncommitted (loser) PageInsert is
        // skipped — never resurrected.
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PageType, SlottedPage};

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Committed page 8 (txid 10 + Commit); loser page 7 (txid 11, no commit).
        // Neither page is on disk — both eager writes were lost by the crash.
        let mut committed = SlottedPage::init(8, PageType::SlottedData);
        committed.insert(b"committed").unwrap();
        committed.update_checksum();
        let mut loser = SlottedPage::init(7, PageType::SlottedData);
        loser.insert(b"loser").unwrap();
        loser.update_checksum();

        {
            let mut wal = WalWriter::open(wal_dir.clone(), &fs).unwrap();
            let mut p8 = 8u64.to_be_bytes().to_vec();
            p8.extend_from_slice(&committed.buf);
            wal.append(&fs, WalRecord::new(RecordType::PageInsert, 10, 0, 0, p8))
                .unwrap();
            wal.append(&fs, WalRecord::new(RecordType::Commit, 10, 0, 0, vec![]))
                .unwrap();
            let mut p7 = 7u64.to_be_bytes().to_vec();
            p7.extend_from_slice(&loser.buf);
            wal.append(&fs, WalRecord::new(RecordType::PageInsert, 11, 0, 0, p7))
                .unwrap();
            wal.sync(&fs).unwrap();
        }

        let mut rwal = WalWriter::open(wal_dir.clone(), &fs).unwrap();
        let recovery = AriesRecovery::new(&fs, &wal_dir, &data_path, 0);
        let result = recovery.recover(&mut rwal).unwrap();
        assert_eq!(result.redo_count, 1, "only the committed page must be redone");

        let handle = fs.open(&data_path, false).unwrap();
        // Page 8 (committed) reconstructed and checksum-valid.
        let mut buf8 = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf8, 8 * PAGE_SIZE as u64).unwrap();
        assert!(
            SlottedPage::verify_checksum_bytes(buf8.as_ref()),
            "committed page must be valid after REDO"
        );
        assert_eq!(SlottedPage::new(buf8).read(0), Some(&b"committed"[..]));
        // Page 7 (loser) was never written: zero-filled by the file extension.
        let mut buf7 = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf7, 7 * PAGE_SIZE as u64).unwrap();
        assert!(
            buf7.iter().all(|&b| b == 0),
            "loser page must not be resurrected by REDO"
        );
    }

    // ── UNDO with before-images (Task 149) ────────────────────────────────────

    #[test]
    fn undo_insert_writes_tombstone() {
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PageType, SlottedPage};

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");

        // Write a page with data (simulating after an insert).
        {
            let mut page = SlottedPage::init(3, PageType::SlottedData);
            page.insert(b"hello").unwrap();
            page.update_checksum();
            let handle = fs.open(&data_path, true).unwrap();
            handle.write_at(&page.buf, 3 * PAGE_SIZE as u64).unwrap();
            handle.sync_data().unwrap();
        }

        // Build a *physical* PageInsert record for page 3 — no before-image (it
        // is an insert).  Physical inserts ARE tombstoned by UNDO; logical
        // NodeInsert records are NOT physically undone — see
        // `undo_skips_logical_entity_records`.
        let mut payload = 3u64.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0xAAu8; 16]); // fake after-image content

        let mut insert_rec = WalRecord::new(RecordType::PageInsert, 42, 0, 0, payload);
        insert_rec.set_lsn(100);

        // Simulate UNDO: apply_inverse should zero-fill the page.
        apply_inverse(&fs, &data_path, &insert_rec, 3, &[]).unwrap();

        let handle = fs.open(&data_path, false).unwrap();
        let mut buf = crate::io::AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 3 * PAGE_SIZE as u64).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0),
            "page must be zeroed after insert undo"
        );
    }

    #[test]
    fn seed_dpt_tolerates_corrupt_checkpoint_payloads() {
        // Regression gate for finding L8 (2026-06-04): a corrupt CheckpointEnd
        // payload must never panic and must not do unbounded work, whatever the
        // claimed count.
        // (a) Huge count with a tiny payload → ignored by the length guard.
        let mut p = u32::MAX.to_be_bytes().to_vec(); // count = 4_294_967_295
        p.extend_from_slice(&[0u8; 8]);
        let mut dpt = HashMap::new();
        seed_dpt_from_checkpoint(&p, &mut dpt);
        assert!(dpt.is_empty(), "huge count must not seed any entries");

        // (b) Assorted short payloads never panic.
        for len in 0..80usize {
            let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(37)).collect();
            let mut d = HashMap::new();
            seed_dpt_from_checkpoint(&payload, &mut d); // must not panic
        }

        // (c) A well-formed payload with count = 2 seeds exactly 2 entries.
        let mut good = 2u32.to_be_bytes().to_vec();
        good.extend_from_slice(&10u64.to_be_bytes());
        good.extend_from_slice(&100u64.to_be_bytes());
        good.extend_from_slice(&20u64.to_be_bytes());
        good.extend_from_slice(&200u64.to_be_bytes());
        let mut d2 = HashMap::new();
        seed_dpt_from_checkpoint(&good, &mut d2);
        assert_eq!(d2.len(), 2);
        assert_eq!(d2[&10].rec_lsn, 100);
        assert_eq!(d2[&20].rec_lsn, 200);
    }

    #[test]
    fn before_image_roundtrips_arbitrary_images() {
        for &len in &[0usize, 1, 7, 100, 4095, PAGE_SIZE] {
            let image: Vec<u8> = (0..len)
                .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
                .collect();
            let mut payload = b"after-image-prefix-bytes".to_vec();
            embed_before_image(&mut payload, &image);
            let got = extract_before_image(&payload).expect("before-image must round-trip");
            assert_eq!(got, &image[..], "round-trip failed for len {len}");
        }
    }

    #[test]
    fn before_image_extract_rejects_coincidental_magic() {
        // Regression gate for finding M25 (2026-06-04): a payload that was never
        // framed by `embed_before_image` must never be mistaken for a
        // before-image, even when its bytes contain — or end in — the BIMG magic.

        // (a) A full page of the magic byte pattern: contains the magic many
        //     times but is not a valid frame.
        let mut page = vec![0u8; PAGE_SIZE];
        for chunk in page.chunks_mut(4) {
            if chunk.len() == 4 {
                chunk.copy_from_slice(&BEFORE_IMAGE_MAGIC.to_be_bytes());
            }
        }
        assert!(extract_before_image(&page).is_none());

        // (b) A payload ending exactly in the magic but with no valid len/crc.
        let mut p = vec![0xABu8; 64];
        p.extend_from_slice(&BEFORE_IMAGE_MAGIC.to_be_bytes());
        assert!(extract_before_image(&p).is_none());

        // (c) A payload ending in [bogus crc][len=32][magic] — magic and a
        //     self-consistent length, but the CRC does not match the bytes.
        let mut q = vec![0xCDu8; 100];
        q.extend_from_slice(&0u32.to_be_bytes()); // wrong crc
        q.extend_from_slice(&32u32.to_be_bytes()); // len = 32
        q.extend_from_slice(&BEFORE_IMAGE_MAGIC.to_be_bytes());
        assert!(extract_before_image(&q).is_none());

        // (d) A genuine PageUpdate-style payload (page_id + full page image) with
        //     no embedded before-image must extract to None.
        let mut upd = 5u64.to_be_bytes().to_vec();
        upd.extend_from_slice(&vec![0x42u8; PAGE_SIZE]); // page image, no frame
        assert!(extract_before_image(&upd).is_none());
    }

    #[test]
    fn undo_skips_logical_entity_records() {
        // Regression gate for reliability-audit finding C2 (2026-06-04): a loser
        // transaction whose chain contains a logical `NodeInsert` must NOT be
        // physically undone.  Physical UNDO would read the `node_id` from the
        // first 8 payload bytes as a `page_id` and zero-fill the page at
        // `node_id * PAGE_SIZE` — here node_id 2, i.e. the bitmap page — silently
        // corrupting committed metadata during recovery.
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PageType, SlottedPage};

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Write a recognizable sentinel page at offset node_id * PAGE_SIZE.
        let node_id: u64 = 2;
        {
            let mut page = SlottedPage::init(node_id, PageType::SlottedData);
            page.insert(b"do-not-touch").unwrap();
            page.update_checksum();
            let handle = fs.open(&data_path, true).unwrap();
            handle
                .write_at(&page.buf, node_id * PAGE_SIZE as u64)
                .unwrap();
            handle.sync_data().unwrap();
        }
        let mut sentinel = AlignedBuffer::zeroed(PAGE_SIZE);
        {
            let handle = fs.open(&data_path, false).unwrap();
            handle
                .read_at(&mut sentinel, node_id * PAGE_SIZE as u64)
                .unwrap();
        }

        // Active loser: Begin(txid=42) -> NodeInsert(txid=42, node_id=2), no Commit.
        let mut begin = WalRecord::new(RecordType::Begin, 42, 0, 0, Vec::new());
        begin.set_lsn(10);
        let mut payload = node_id.to_be_bytes().to_vec(); // first 8 bytes = entity id
        payload.extend_from_slice(&[0xAAu8; 32]); // fake record bytes (no before-image)
        let mut insert = WalRecord::new(RecordType::NodeInsert, 42, 0, 10, payload);
        insert.set_lsn(20);

        let mut wal = WalWriter::open(wal_dir.clone(), &fs).unwrap();
        let recovery = AriesRecovery::new(&fs, &wal_dir, &data_path, 0);
        let result = recovery
            .recover_from_slice(&[begin, insert], &mut wal)
            .unwrap();

        // The logical NodeInsert must not have been physically undone.
        assert_eq!(
            result.undo_count, 0,
            "logical NodeInsert must not be physically undone"
        );
        let handle = fs.open(&data_path, false).unwrap();
        let mut after = AlignedBuffer::zeroed(PAGE_SIZE);
        handle
            .read_at(&mut after, node_id * PAGE_SIZE as u64)
            .unwrap();
        assert_eq!(
            &after[..],
            &sentinel[..],
            "page at node_id*PAGE_SIZE must be untouched by UNDO of a logical record"
        );
    }

    #[test]
    fn undo_update_restores_before_image() {
        use crate::io::posix::PosixFileSystem;
        

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join("data.db");

        // Before-image: page filled with 0xBB.
        let mut before_image = vec![0xBBu8; PAGE_SIZE];
        // After-image: page filled with 0xCC (the update).
        let after_image = vec![0xCCu8; PAGE_SIZE];

        // Write after-image to disk (current state = post-update).
        {
            let handle = fs.open(&data_path, true).unwrap();
            handle.write_at(&after_image, 5 * PAGE_SIZE as u64).unwrap();
            handle.sync_data().unwrap();
        }

        // Build a PageUpdate WAL record with embedded before-image.
        let mut payload = 5u64.to_be_bytes().to_vec();
        payload.extend_from_slice(&after_image); // after-image content
        embed_before_image(&mut payload, &before_image);

        let mut update_rec = WalRecord::new(RecordType::PageUpdate, 1, 0, 0, payload);
        update_rec.set_lsn(200);

        // Undo: should restore before_image.
        let (page_id, ai) = extract_page_and_image(&update_rec).unwrap();
        assert_eq!(page_id, 5);
        apply_inverse(&fs, &data_path, &update_rec, page_id, ai).unwrap();

        let handle = fs.open(&data_path, false).unwrap();
        let mut buf = crate::io::AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 5 * PAGE_SIZE as u64).unwrap();
        before_image[0..8].copy_from_slice(&0u64.to_be_bytes()); // LSN field zeroed by apply_inverse
        // Verify the before_image content is restored (first non-LSN byte should be 0xBB).
        assert_eq!(buf[8], 0xBB, "before-image must be restored by undo");
    }

    // ── Multi-segment recovery (Task 150) ────────────────────────────────────

    #[test]
    fn multi_segment_recovery_reads_all_segments() {
        use crate::io::posix::PosixFileSystem;

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Write records into two separate segment files.
        // Segment 0: Begin txid=1 at LSN=1.
        {
            let mut r = WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]);
            r.set_lsn(crate::wal::writer::make_lsn(0, 1));
            let bytes = r.encode();
            let handle = fs.open(&wal_dir.join("wal-000000000"), true).unwrap();
            handle.write_at(&bytes, 0).unwrap();
            handle.sync_data().unwrap();
        }
        // Segment 1: Commit txid=1 at LSN=(1<<32)|1.
        {
            let mut r = WalRecord::new(
                RecordType::Commit,
                1,
                0,
                crate::wal::writer::make_lsn(0, 1),
                vec![],
            );
            r.set_lsn(crate::wal::writer::make_lsn(1, 1));
            let bytes = r.encode();
            let handle = fs.open(&wal_dir.join("wal-000000001"), true).unwrap();
            handle.write_at(&bytes, 0).unwrap();
            handle.sync_data().unwrap();
        }

        let data_path = dir.path().join("data.db");
        let recovery = AriesRecovery::new(&fs, &wal_dir, &data_path, 0);
        let records = recovery.load_all_segments(&fs, &wal_dir).unwrap();

        assert_eq!(records.len(), 2, "should read records from both segments");
        assert_eq!(records[0].record_type, RecordType::Begin);
        assert_eq!(records[1].record_type, RecordType::Commit);
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
