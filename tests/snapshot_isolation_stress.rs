//! Concurrent stress tests for storage-layer snapshot isolation (MVCC).
//!
//! These tests validate the guarantee that underpins eager-update isolation
//! (Task 29): a transaction reads the graph at a fixed point-in-time snapshot,
//! so concurrent writes — and a statement's own later writes — never become
//! visible to it.  This is the storage-level mechanism that, together with the
//! planner's `Eager` barrier, prevents the Halloween problem
//! (`MATCH (n) CREATE (n)-[:R]->(m)` must not re-observe the nodes it creates).
//!
//! A tuple "created by" transaction `w` is modelled by its `xmin = w` (its
//! version header's creating-transaction id); a live tuple has
//! `xmax = TX_ID_INVALID`.  `Snapshot::is_visible` is the production visibility
//! evaluator under test.

use rgraph::txn::snapshot::Snapshot;
use rgraph::txn::state::GlobalTxState;
use rgraph::txn::txid::{TxId, TX_ID_INVALID};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// A live tuple created by `creator`.
fn live(snap: &Snapshot, creator: TxId) -> bool {
    snap.is_visible(creator, TX_ID_INVALID, false, false)
}

/// A reader's snapshot must never observe a tuple created by a transaction that
/// began at or after the snapshot was taken — even while many such writers
/// commit concurrently (snapshot stability / repeatable read).
#[test]
fn snapshot_is_stable_under_concurrent_writers() {
    let state = Arc::new(GlobalTxState::new());

    // A pre-existing committed tuple that the reader MUST always see.
    let (pre_txid, _) = state.begin_tx();
    state.commit_tx(pre_txid);

    // The reader captures its snapshot now, after `pre_txid` committed.
    let (reader_txid, reader_snap) = state.begin_tx();
    let reader_snap = Arc::new(reader_snap);

    // Sanity: the pre-existing tuple is visible at the start.
    assert!(live(&reader_snap, pre_txid));

    let stop = Arc::new(AtomicBool::new(false));
    let created = Arc::new(Mutex::new(Vec::<TxId>::new()));

    // Writer threads: continuously begin + commit, each "creating" a tuple
    // whose xmin is its own (post-snapshot) txid.
    let mut writers = Vec::new();
    for _ in 0..8 {
        let state = Arc::clone(&state);
        let stop = Arc::clone(&stop);
        let created = Arc::clone(&created);
        writers.push(thread::spawn(move || {
            let mut local = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let (w, _snap) = state.begin_tx();
                state.commit_tx(w);
                local.push(w);
            }
            created.lock().unwrap().extend(local);
        }));
    }

    // Reader thread: hammer the visibility check while the writers race.
    let violations = Arc::new(AtomicU64::new(0));
    let reader = {
        let snap = Arc::clone(&reader_snap);
        let violations = Arc::clone(&violations);
        thread::spawn(move || {
            for _ in 0..200_000 {
                // The pre-existing committed tuple must ALWAYS stay visible.
                if !live(&snap, pre_txid) {
                    violations.fetch_add(1, Ordering::Relaxed);
                }
                // Any tuple whose creator started at/after the snapshot boundary
                // must NEVER be visible, regardless of concurrent commits.
                if live(&snap, snap.xmax) || live(&snap, snap.xmax + 4096) {
                    violations.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };

    reader.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }

    assert_eq!(
        violations.load(Ordering::Relaxed),
        0,
        "snapshot isolation was violated under concurrency"
    );

    // Every tuple actually created by the concurrent writers is invisible to
    // the long-lived reader snapshot.
    let created = created.lock().unwrap();
    assert!(!created.is_empty(), "writers should have committed work");
    for &w in created.iter() {
        assert!(
            w >= reader_snap.xmax,
            "writer {w} should have started after the reader snapshot (xmax={})",
            reader_snap.xmax
        );
        assert!(
            !live(&reader_snap, w),
            "reader snapshot must not observe concurrently-created tuple {w}"
        );
    }

    state.commit_tx(reader_txid);
}

/// Under concurrent churn, each transaction's snapshot is independently
/// isolated: it sees its own writes and any tuple committed strictly before its
/// snapshot, but not tuples from transactions that start later.
#[test]
fn per_transaction_snapshots_are_isolated_under_load() {
    let state = Arc::new(GlobalTxState::new());
    let failures = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..16 {
        let state = Arc::clone(&state);
        let failures = Arc::clone(&failures);
        handles.push(thread::spawn(move || {
            for _ in 0..500 {
                // A writer commits a tuple.
                let (w, _) = state.begin_tx();
                state.commit_tx(w);

                // A reader that starts AFTER `w` committed must observe `w`...
                let (reader, snap) = state.begin_tx();
                if !live(&snap, w) {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                // ...and must observe its own writes...
                if !live(&snap, reader) {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                // ...but must not observe a tuple from a later transaction.
                if live(&snap, snap.xmax) {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                state.commit_tx(reader);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        failures.load(Ordering::Relaxed),
        0,
        "per-transaction snapshot isolation was violated under load"
    );
}

/// A transaction still *active* when a reader's snapshot is taken stays
/// invisible to that reader even after the writer later commits — the classic
/// snapshot-isolation requirement that distinguishes it from read-committed.
#[test]
fn active_writer_at_snapshot_time_stays_invisible_after_commit() {
    let state = Arc::new(GlobalTxState::new());

    // Writer begins but does NOT commit yet.
    let (writer, _writer_snap) = state.begin_tx();

    // Reader takes its snapshot while the writer is still active.
    let (reader, reader_snap) = state.begin_tx();

    // The writer's tuple is invisible: it was active at snapshot time.
    assert!(
        !live(&reader_snap, writer),
        "an active writer's tuple must be invisible to a concurrent snapshot"
    );

    // The writer now commits — but the reader's snapshot is immutable, so the
    // tuple must remain invisible (repeatable read).
    state.commit_tx(writer);
    assert!(
        !live(&reader_snap, writer),
        "committing after the snapshot must not retroactively expose the tuple"
    );

    state.commit_tx(reader);
}
