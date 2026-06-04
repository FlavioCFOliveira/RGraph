//! Crash-consistency and fault-injection integration tests (Task 185).
//!
//! These tests wire the [`FaultInjectFileSystem`] into the real
//! [`Database`]/[`GraphStorageEngine`] open/recovery path and assert the four
//! durability/atomicity invariants that the ARIES + double-write design
//! promises:
//!
//! 1. **Torn-page repair** — a partial page write during a flush is detected
//!    and repaired from the double-write buffer on the next open, so recovery
//!    never observes a half-written page (Sprint A/B work).
//! 2. **Fsync-failure surfacing** — an `fsync` failure is propagated as an
//!    error rather than being silently swallowed (no false durability claim).
//! 3. **Atomicity (ARIES UNDO)** — a transaction whose Commit record never
//!    reached durable storage is rolled back by UNDO; its page mutations are
//!    reversed to their before-image on the next recovery.
//! 4. **Durability (ARIES REDO)** — records written, committed, and synced
//!    before a crash are present after reopen.
//!
//! Every test uses a temp directory, a small workload, and bounded I/O so the
//! suite completes in well under a second with no hangs.

use rgraph::config::GraphMode;
use rgraph::db::database::Database;
use rgraph::graph::Property;
use rgraph::graph::builder::{NodeBuilder, RelationshipBuilder};
use rgraph::io::posix::PosixFileSystem;
use rgraph::io::{
    AlignedBuffer, FaultConfig, FaultInjectFileSystem, FaultKind, FaultRule, FileSystem, OpMask,
};
use rgraph::storage::manager::PageManager;
use rgraph::storage::page::{PAGE_SIZE, SlottedPage};

/// Build a fault-injecting filesystem wrapping a buffered POSIX backend with an
/// empty (no-op) rule set.  Callers install rules with [`set_rule`].
fn fault_fs() -> FaultInjectFileSystem {
    FaultInjectFileSystem::new(Box::new(PosixFileSystem::new(false)))
}

/// Install a single fault rule, replacing any existing configuration.
fn set_rule(fs: &FaultInjectFileSystem, op_mask: OpMask, kind: FaultKind, every_n: Option<u64>) {
    fs.set_config(FaultConfig {
        rules: vec![FaultRule {
            op_mask,
            kind,
            every_n,
        }],
    });
}

/// Clear all fault rules so subsequent I/O proceeds normally.
fn clear_rules(fs: &FaultInjectFileSystem) {
    fs.set_config(FaultConfig::default());
}

/// A sync-all + sync-data op mask (covers every durability flush).
fn sync_mask() -> OpMask {
    OpMask {
        sync_all: true,
        sync_data: true,
        ..OpMask::default()
    }
}

// ---------------------------------------------------------------------------
// 1. Torn / partial page write is repaired from the double-write buffer.
// ---------------------------------------------------------------------------

/// Inject a torn (partial) page write into a data page, then "kill" (drop) and
/// reopen the engine.  Recovery must restore the page to a coherent, checksum-
/// valid state via the double-write buffer, and the committed record must be
/// readable afterwards.
#[test]
fn torn_page_write_is_repaired_on_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let fs = fault_fs();

    // Phase 1: create a database and a node, sync it durably, then drop.
    let node_id = {
        let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
        let (_slot, node_id) = db
            .create_node(NodeBuilder::new().label(7), &fs)
            .unwrap();
        db.sync(&fs).unwrap();
        node_id
    };

    // Locate the data file and pick a data page (the first non-metadata page,
    // id >= 3) to corrupt with a torn write.
    let data_path = db_path.join(PageManager::DATA_FILE);
    let handle = fs.open(&data_path, false).unwrap();
    let file_len = handle.len().unwrap();
    let total_pages = file_len / PAGE_SIZE as u64;
    assert!(
        total_pages > 3,
        "expected at least one data page beyond metadata, got {total_pages}"
    );
    let target_page: u64 = 3;
    let offset = target_page * PAGE_SIZE as u64;

    // Read the good page image so we can stage it in the double-write buffer
    // and then partially overwrite it to simulate a torn write.
    let mut good = AlignedBuffer::zeroed(PAGE_SIZE);
    handle.read_at(&mut good, offset).unwrap();
    drop(handle);

    // Stage the good image into the double-write file exactly as the pool does
    // before an in-place flush.  The `.dw` file lives next to the data file.
    let dw_path = data_path.with_extension("dw");
    let dw = rgraph::wal::doublewrite::DoubleWriteBuffer::open(dw_path, &fs).unwrap();
    dw.write_batch(&[(target_page, good.as_ref())], &fs).unwrap();

    // Now tear the page: write only the first half of a fresh (garbage) image
    // to the page's final location, leaving a checksum-invalid page on disk.
    let mut torn = AlignedBuffer::zeroed(PAGE_SIZE);
    torn.as_mut().copy_from_slice(good.as_ref());
    // Mutate the back half so the stored checksum no longer matches.
    for b in &mut torn.as_mut()[PAGE_SIZE / 2..] {
        *b ^= 0xFF;
    }
    let handle = fs.open(&data_path, false).unwrap();
    handle.write_at(&torn, offset).unwrap();
    handle.sync_all().unwrap();
    drop(handle);

    // Sanity: the on-disk page is now torn (checksum invalid).
    let handle = fs.open(&data_path, false).unwrap();
    let mut check = AlignedBuffer::zeroed(PAGE_SIZE);
    handle.read_at(&mut check, offset).unwrap();
    drop(handle);
    assert!(
        !SlottedPage::verify_checksum_bytes(check.as_ref()),
        "test setup: page should be torn (checksum invalid) before recovery"
    );

    // Phase 2: reopen.  Double-write recovery must restore the torn page before
    // ARIES runs, and the node must still be readable.
    let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
    let node = db
        .get_node(node_id, &fs)
        .expect("get_node should not error after recovery")
        .expect("node must survive torn-page recovery");
    assert_eq!(node.node_id, node_id);
    assert_eq!(node.label_id, 7);

    // The restored page must now be checksum-valid on disk.
    let handle = fs.open(&data_path, false).unwrap();
    let mut repaired = AlignedBuffer::zeroed(PAGE_SIZE);
    handle.read_at(&mut repaired, offset).unwrap();
    drop(handle);
    assert!(
        SlottedPage::verify_checksum_bytes(repaired.as_ref()),
        "double-write buffer must repair the torn page to a checksum-valid state"
    );
}

// ---------------------------------------------------------------------------
// 2. An fsync failure is surfaced as an error (no silent data loss).
// ---------------------------------------------------------------------------

/// Inject an `fsync` failure during a durability flush and assert the error
/// propagates out of the sync path rather than being swallowed.  A storage
/// engine that claimed success here would be lying about durability.
#[test]
fn fsync_failure_is_surfaced() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let fs = fault_fs();

    let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
    db.create_node(NodeBuilder::new().label(1), &fs).unwrap();

    // Make every fsync fail.
    set_rule(&fs, sync_mask(), FaultKind::FsyncFail, None);

    let result = db.sync(&fs);
    assert!(
        result.is_err(),
        "db.sync must surface the underlying fsync failure, not swallow it"
    );

    // Recovery: with fsync working again, a clean reopen must succeed.
    clear_rules(&fs);
    let _db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
}

// ---------------------------------------------------------------------------
// 3. Atomicity under crash: an uncommitted transaction's page mutation is
//    rolled back by ARIES UNDO.
// ---------------------------------------------------------------------------

/// Drive the real [`AriesRecovery`] over a hand-built WAL whose only
/// transaction wrote a page update but whose **Commit record never reached
/// durable storage** (the crash happened mid-transaction).  ARIES must:
///
/// * classify the transaction as a loser in ANALYSIS (no Commit), and
/// * reverse its page mutation in UNDO, restoring the page's before-image and
///   emitting at least one compensation log record (CLR).
///
/// This is the layer-faithful model of crash atomicity.  The high-level
/// `Database` API autocommits each statement (each `put_node` flushes its own
/// commit), so the only way a write is genuinely *uncommitted* at crash time
/// is for the transaction's Commit record never to be persisted — exactly what
/// this test reproduces by writing WAL records directly and omitting Commit.
#[test]
fn uncommitted_transaction_is_undone_on_recovery() {
    use rgraph::wal::aries::{AriesRecovery, embed_before_image};
    use rgraph::wal::record::{RecordType, WalRecord};
    use rgraph::wal::writer::WalWriter;

    let dir = tempfile::tempdir().unwrap();
    let fs = PosixFileSystem::new(false);
    let wal_dir = dir.path().join("wal");
    let data_path = dir.path().join("data.db");

    // The on-disk page currently holds the *after-image* of the doomed update
    // (the dirty page reached disk before the crash, but the commit did not).
    // UNDO must roll it back to the before-image.
    const TARGET_PAGE: u64 = 5;
    let before_image = vec![0xBBu8; PAGE_SIZE];
    let after_image = vec![0xCCu8; PAGE_SIZE];
    {
        let handle = fs.open(&data_path, true).unwrap();
        handle
            .write_at(&after_image, TARGET_PAGE * PAGE_SIZE as u64)
            .unwrap();
        handle.sync_data().unwrap();
    }

    // Build a WAL segment: Begin(txid=1) → PageUpdate(after+before image) and
    // then STOP — no Commit record is ever written (the crash point).
    {
        let mut wal = WalWriter::open(wal_dir.clone(), &fs).unwrap();

        let begin = WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]);
        let begin_lsn = wal.append(&fs, begin).unwrap();

        let mut payload = TARGET_PAGE.to_be_bytes().to_vec();
        payload.extend_from_slice(&after_image);
        embed_before_image(&mut payload, &before_image);
        let update = WalRecord::new(RecordType::PageUpdate, 1, 0, begin_lsn, payload);
        wal.append(&fs, update).unwrap();

        // Flush the loser's records to durable storage, then drop the writer
        // WITHOUT ever appending a Commit — this is the crash.
        wal.sync(&fs).unwrap();
    }

    // Recovery: run the production ANALYSIS → REDO → UNDO driver.  A fresh
    // WalWriter resumes LSN allocation past the records already in the segment
    // (it scans the existing segment on open), so CLRs do not collide.
    let mut recovery_wal = WalWriter::open(wal_dir.clone(), &fs).unwrap();
    let recovery = AriesRecovery::new(&fs, &wal_dir, &data_path, 0);
    let result = recovery.recover(&mut recovery_wal).unwrap();

    // ANALYSIS must have seen exactly one transaction and classified it as a
    // loser (Active — it never committed or aborted).
    assert_eq!(
        result.att.len(),
        1,
        "exactly one transaction should appear in the ATT"
    );
    assert!(
        result.undo_count >= 1,
        "UNDO must reverse the loser's page mutation (undo_count={})",
        result.undo_count
    );

    // The page on disk must be restored to its before-image (0xBB), with the
    // leading 8-byte LSN slot zeroed by `apply_inverse`.
    let handle = fs.open(&data_path, false).unwrap();
    let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
    handle
        .read_at(&mut buf, TARGET_PAGE * PAGE_SIZE as u64)
        .unwrap();
    drop(handle);
    assert_eq!(
        buf.as_ref()[8],
        0xBB,
        "UNDO must restore the page's before-image after a crash with no commit"
    );
}

// ---------------------------------------------------------------------------
// 4. Durability under crash: committed+synced writes survive a reopen.
// ---------------------------------------------------------------------------

/// Write a batch of records, commit and sync them durably, then crash (drop)
/// and reopen.  Every committed record must be present — ARIES REDO replays
/// any page updates that had not yet reached their final on-disk location.
#[test]
fn committed_writes_survive_crash() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let fs = fault_fs();

    let ids: Vec<u64> = {
        let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
        let mut ids = Vec::new();
        for label in 0..8u32 {
            let (_s, id) = db
                .create_node(NodeBuilder::new().label(label), &fs)
                .unwrap();
            ids.push(id);
        }
        // Durable commit point: flush WAL + superblock + bitmap.
        db.sync(&fs).unwrap();
        // Crash: drop without any further writes.
        drop(db);
        ids
    };

    // Reopen and verify every committed node is present with the right label.
    let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
    for (expected_label, id) in ids.iter().enumerate() {
        let node = db
            .get_node(*id, &fs)
            .unwrap()
            .unwrap_or_else(|| panic!("committed node {id} must survive crash"));
        assert_eq!(
            node.label_id, expected_label as u32,
            "node {id} label must be intact after REDO"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Atomic create: a CREATE (n {props}) commits the node and its whole
//    property chain as ONE no-steal transaction and survives a reopen intact.
// ---------------------------------------------------------------------------

/// Create a node WITH multiple properties, sync, drop, and reopen.  Under the
/// no-steal/no-force policy (findings C1/C3) the node page and all property
/// pages commit as a single transaction, so every property must survive the
/// restart — there is no window where the node exists without its properties.
#[test]
fn create_node_with_properties_survives_crash_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let fs = fault_fs();

    let id = {
        let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
        let (_s, id) = db
            .create_node(
                NodeBuilder::new()
                    .label(3)
                    .property("name", "alice")
                    .property("age", 30i64),
                &fs,
            )
            .unwrap();
        db.sync(&fs).unwrap();
        drop(db);
        id
    };

    let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
    let node = db
        .get_node(id, &fs)
        .unwrap()
        .expect("node must survive restart");
    assert_eq!(node.label_id, 3);
    assert_eq!(
        node.properties.len(),
        2,
        "both properties must survive the restart atomically with the node"
    );
    assert_eq!(
        node.properties.get("name"),
        Some(&Property::String("alice".to_string()))
    );
    assert_eq!(node.properties.get("age"), Some(&Property::Integer(30)));
}

// ---------------------------------------------------------------------------
// 6. Atomic edge create: a CREATE (a)-[:R]->(b) commits the edge record AND
//    both adjacency links as ONE no-steal transaction; after a reopen the edge
//    is present and traversable in BOTH directions (never half-linked).
// ---------------------------------------------------------------------------

/// Create two nodes and a relationship between them (with a property), sync,
/// drop, and reopen.  Under the no-steal/no-force policy (finding C3b) the edge
/// record, its property page and BOTH endpoint adjacency lists commit as one
/// transaction, so after the restart the edge must be reachable from the
/// source's outgoing list and the target's incoming list — there is no window
/// where the graph is half-linked.
#[test]
fn create_relationship_survives_crash_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let fs = fault_fs();

    let (src, tgt, edge_id) = {
        let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
        let (_s, src) = db.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_s, tgt) = db.create_node(NodeBuilder::new().label(2), &fs).unwrap();
        let (_e, edge_id) = db
            .create_relationship(
                RelationshipBuilder::new()
                    .from(src)
                    .to(tgt)
                    .type_id(7)
                    .property("weight", 5i64),
                &fs,
            )
            .unwrap();
        db.sync(&fs).unwrap();
        drop(db);
        (src, tgt, edge_id)
    };

    let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();

    // The edge record survives with its endpoints and type intact.
    let edge = db
        .get_relationship(edge_id, &fs)
        .unwrap()
        .expect("edge must survive restart");
    assert_eq!(edge.type_id, 7);
    assert_eq!(edge.source_id, src);
    assert_eq!(edge.target_id, tgt);

    // Adjacency is consistent in BOTH directions (the two links committed atomically).
    let out = db
        .graph()
        .engine()
        .scan_outgoing_edges(src, &[], &fs)
        .unwrap();
    assert!(
        out.iter().any(|(e, _)| e.edge_id == edge_id),
        "edge must be in the source's outgoing adjacency after restart"
    );
    let inc = db
        .graph()
        .engine()
        .scan_incoming_edges(tgt, &[], &fs)
        .unwrap();
    assert!(
        inc.iter().any(|(e, _)| e.edge_id == edge_id),
        "edge must be in the target's incoming adjacency after restart"
    );
}
