//! Global transaction state manager.
//!
//! [`GlobalTxState`] maintains the authoritative list of active transactions
//! and the derived `global_xmin` / `global_xmax` values.  It is the single
//! source of truth for snapshot acquisition.
//!
//! # Concurrency model
//!
//! * `global_xmax` is a monotonically increasing [`AtomicU64`]: readers can
//!   load it without a lock.
//! * `global_xmin` is an [`AtomicU64`] updated eagerly whenever the active
//!   set changes (see [`GlobalTxState::advance_xmin`]).
//! * The active set is protected by a [`std::sync::Mutex`].  The critical
//!   section is intentionally minimal: insert/remove from a `BTreeSet` plus
//!   a snapshot of the values needed for [`Snapshot`] construction.
//!
//! Under extremely high concurrency the Mutex could become a bottleneck.
//! A seqlock or RCU scheme would reduce contention, but would require
//! `unsafe` and adds complexity that is not yet warranted.  The design is
//! structured so that the Mutex guard is **never held across `.await`** or
//! any other blocking operation.

use crate::txn::{
    snapshot::Snapshot,
    txid::{TxId, TxIdAllocator, TX_ID_INVALID},
};
use std::collections::{BTreeSet, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

/// Global transaction state, shared across all active transactions.
pub struct GlobalTxState {
    /// The lowest TxId that is currently active (or `TX_ID_BOOTSTRAP` if
    /// no transactions are running).  Updated eagerly on begin/commit/abort.
    global_xmin: AtomicU64,
    /// The next TxId that has not yet been assigned.  Monotonically
    /// increasing; equals `allocator.peek_next()` at all times.
    global_xmax: AtomicU64,
    /// Protected state: the set of active TxIds and the TxId allocator.
    inner: Mutex<Inner>,
    /// Shared commit/abort log: TxIds that aborted.  Consulted by snapshot
    /// visibility so a tuple created by an aborted transaction stays invisible
    /// even after `global_xmin` advances past it (finding M14).  Grows until a
    /// future vacuum can forget entries whose versions are all gone.
    aborted: Arc<Mutex<HashSet<TxId>>>,
}

struct Inner {
    /// Sorted set of in-flight transaction IDs.
    active: BTreeSet<TxId>,
    /// The TxID allocator.
    allocator: TxIdAllocator,
}

impl GlobalTxState {
    /// Create a fresh global state (new database or after recovery).
    pub fn new() -> Self {
        let allocator = TxIdAllocator::new();
        let xmax = allocator.peek_next();
        Self {
            global_xmin: AtomicU64::new(xmax),
            global_xmax: AtomicU64::new(xmax),
            inner: Mutex::new(Inner {
                active: BTreeSet::new(),
                allocator,
            }),
            aborted: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Create a global state for recovery, restoring the TxID counter from
    /// a system-page byte slice.
    pub fn recover(system_page: &[u8]) -> Self {
        let allocator = TxIdAllocator::recover(system_page);
        let xmax = allocator.peek_next();
        Self {
            global_xmin: AtomicU64::new(xmax),
            global_xmax: AtomicU64::new(xmax),
            inner: Mutex::new(Inner {
                active: BTreeSet::new(),
                allocator,
            }),
            aborted: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Begin a new transaction.
    ///
    /// Returns `(txid, snapshot)` where `txid` is the newly assigned
    /// transaction identifier and `snapshot` is the point-in-time view of
    /// committed data that this transaction will see.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (indicates a previous panic
    /// while holding the lock — a fatal condition for the engine).
    pub fn begin_tx(&self) -> (TxId, Snapshot) {
        let mut guard = self
            .inner
            .lock()
            .expect("INVARIANT: GlobalTxState mutex must not be poisoned");

        // Allocate a new TxId while holding the lock so that the snapshot
        // xmax is consistent with the txid we return.
        let txid = guard.allocator.allocate();
        let xmax = guard.allocator.peek_next();

        // Compute xmin: the lowest active txid, or xmax if none are active.
        let xmin = guard
            .active
            .iter()
            .next()
            .copied()
            .unwrap_or(txid);

        // Snapshot the active set (excluding the new txid which has not yet
        // made any writes).
        let active_snapshot: Vec<TxId> = guard.active.iter().copied().collect();

        // Register this transaction as active.
        guard.active.insert(txid);

        // Update the atomic xmax (monotonically increasing).
        self.global_xmax.store(xmax, Ordering::SeqCst);

        // Update xmin: the new transaction might be lower than the current
        // global_xmin if this is the very first transaction.
        self.global_xmin
            .store(xmin.min(txid), Ordering::SeqCst);

        drop(guard);

        let snapshot =
            Snapshot::new(xmin, xmax, active_snapshot, txid).with_aborted(Arc::clone(&self.aborted));
        (txid, snapshot)
    }

    /// Mark a transaction as committed and remove it from the active set.
    ///
    /// This advances `global_xmin` if the committed transaction was the
    /// oldest active one.
    pub fn commit_tx(&self, txid: TxId) {
        self.remove_active(txid);
    }

    /// Mark a transaction as aborted and remove it from the active set.
    ///
    /// The TxId is recorded in the abort log so its versions stay invisible to
    /// every snapshot, even after `global_xmin` advances past it (finding M14).
    pub fn abort_tx(&self, txid: TxId) {
        self.aborted
            .lock()
            .expect("INVARIANT: abort log mutex must not be poisoned")
            .insert(txid);
        self.remove_active(txid);
    }

    /// Remove `txid` from the active set and advance `global_xmin`.
    fn remove_active(&self, txid: TxId) {
        let mut guard = self
            .inner
            .lock()
            .expect("INVARIANT: GlobalTxState mutex must not be poisoned");

        guard.active.remove(&txid);

        // Advance global_xmin to the lowest remaining active txid.
        let new_xmin = guard
            .active
            .iter()
            .next()
            .copied()
            .unwrap_or_else(|| self.global_xmax.load(Ordering::SeqCst));

        drop(guard);

        self.advance_xmin(new_xmin);
    }

    /// Atomically advance `global_xmin` to at least `candidate`.
    ///
    /// We only ever *advance* (increase) xmin, never retreat it.
    fn advance_xmin(&self, candidate: TxId) {
        // CAS loop to monotonically increase global_xmin.
        let mut current = self.global_xmin.load(Ordering::SeqCst);
        loop {
            if candidate <= current {
                break;
            }
            match self.global_xmin.compare_exchange_weak(
                current,
                candidate,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Acquire a snapshot of the current state without starting a transaction.
    ///
    /// Useful for read-only queries or for constructing a fresh snapshot
    /// mid-transaction (e.g., for `SNAPSHOT ISOLATION` refresh).
    pub fn active_snapshot(&self) -> Snapshot {
        let guard = self
            .inner
            .lock()
            .expect("INVARIANT: GlobalTxState mutex must not be poisoned");

        let xmax = guard.allocator.peek_next();
        let xmin = guard
            .active
            .iter()
            .next()
            .copied()
            .unwrap_or(xmax);
        let active: Vec<TxId> = guard.active.iter().copied().collect();

        drop(guard);

        Snapshot::new(xmin, xmax, active, TX_ID_INVALID).with_aborted(Arc::clone(&self.aborted))
    }

    /// Return the current global xmin (read from the atomic — no lock needed).
    pub fn global_xmin(&self) -> TxId {
        self.global_xmin.load(Ordering::SeqCst)
    }

    /// Return the current global xmax (read from the atomic — no lock needed).
    pub fn global_xmax(&self) -> TxId {
        self.global_xmax.load(Ordering::SeqCst)
    }

    /// Persist the TxID counter to a system-page byte slice.
    pub fn persist_txid_counter(&self, page: &mut [u8]) {
        let guard = self
            .inner
            .lock()
            .expect("INVARIANT: GlobalTxState mutex must not be poisoned");
        guard.allocator.persist(page);
    }
}

impl Default for GlobalTxState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::txn::txid::TX_ID_BOOTSTRAP;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn fresh_state_has_no_active_txns() {
        let state = GlobalTxState::new();
        let snap = state.active_snapshot();
        // Fresh state should have no active transactions.
        assert!(!snap.is_active(TX_ID_BOOTSTRAP));
    }

    /// Regression gate for finding M14 (Task 225, 2026-06-05): a tuple created by
    /// an aborted transaction must be invisible to later snapshots even after
    /// global_xmin advances past the aborted TxId, while a committed creator
    /// below xmin stays visible.
    #[test]
    fn aborted_creator_is_invisible_after_xmin_advances() {
        use crate::txn::txid::TX_ID_INVALID;

        let state = GlobalTxState::new();
        let (t1, _) = state.begin_tx();
        let (t2, _) = state.begin_tx();
        let (_t3, _) = state.begin_tx();

        // t2 commits, t1 aborts; the lowest remaining active txn is t3.
        state.commit_tx(t2);
        state.abort_tx(t1);

        // A later snapshot has xmin = t3, so both t1 and t2 are below xmin.
        let (_t4, s4) = state.begin_tx();
        assert!(
            t1 < s4.xmin && t2 < s4.xmin,
            "xmin must have advanced past both finished txns"
        );

        // The aborted creator t1 is invisible despite being below xmin.
        assert!(
            !s4.is_visible(t1, TX_ID_INVALID, false, false),
            "a tuple created by an aborted txn must be invisible even below xmin"
        );
        // The committed creator t2 (below xmin, not in the abort log) is visible.
        assert!(
            s4.is_visible(t2, TX_ID_INVALID, false, false),
            "a committed creator below xmin must remain visible"
        );
    }

    #[test]
    fn begin_tx_returns_unique_txids() {
        let state = GlobalTxState::new();
        let (t1, _) = state.begin_tx();
        let (t2, _) = state.begin_tx();
        let (t3, _) = state.begin_tx();
        assert_ne!(t1, TX_ID_INVALID);
        assert_ne!(t1, t2);
        assert_ne!(t2, t3);
        assert!(t1 < t2);
        assert!(t2 < t3);
    }

    #[test]
    fn commit_removes_from_active() {
        let state = GlobalTxState::new();
        let (txid, _snap) = state.begin_tx();
        let snap_before = state.active_snapshot();
        assert!(snap_before.is_active(txid));

        state.commit_tx(txid);
        let snap_after = state.active_snapshot();
        assert!(!snap_after.is_active(txid));
    }

    #[test]
    fn abort_removes_from_active() {
        let state = GlobalTxState::new();
        let (txid, _snap) = state.begin_tx();
        state.abort_tx(txid);
        let snap = state.active_snapshot();
        assert!(!snap.is_active(txid));
    }

    #[test]
    fn global_xmin_advances_after_oldest_commits() {
        let state = GlobalTxState::new();
        let (t1, _) = state.begin_tx();
        let (t2, _) = state.begin_tx();
        let (t3, _) = state.begin_tx();

        // xmin is t1 (oldest active).
        assert!(state.global_xmin() <= t1);

        state.commit_tx(t1);
        // After t1 commits, xmin should advance to t2.
        let xmin = state.global_xmin();
        assert!(xmin >= t2, "xmin={xmin} should be >= t2={t2}");

        state.commit_tx(t2);
        let xmin2 = state.global_xmin();
        assert!(xmin2 >= t3, "xmin={xmin2} should be >= t3={t3}");

        state.commit_tx(t3);
        // All committed — xmin should advance past t3.
        let xmin3 = state.global_xmin();
        assert!(xmin3 > t3, "xmin={xmin3} should be > t3={t3}");
    }

    #[test]
    fn snapshot_xmax_is_consistent_with_next_txid() {
        let state = GlobalTxState::new();
        let (t1, snap1) = state.begin_tx();
        let (t2, snap2) = state.begin_tx();
        // snap1.xmax should be < t2 (t2 started after snap1 was taken).
        assert!(snap1.xmax <= t2, "snap1.xmax={} t2={t2}", snap1.xmax);
        // snap2.xmax should be > t2.
        assert!(snap2.xmax > t2, "snap2.xmax={} t2={t2}", snap2.xmax);
        let _ = t1;
    }

    #[test]
    fn concurrent_begin_commit_does_not_panic() {
        let state = Arc::new(GlobalTxState::new());
        let n_threads = 8;
        let n_ops = 200;

        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let s = Arc::clone(&state);
                thread::spawn(move || {
                    for _ in 0..n_ops {
                        let (txid, _snap) = s.begin_tx();
                        s.commit_tx(txid);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        // After all commits, no active transactions remain.
        let snap = state.active_snapshot();
        // The active list should be empty.
        assert!(!snap.is_active(TX_ID_BOOTSTRAP));
    }

    #[test]
    fn persist_and_recover() {
        let state = GlobalTxState::new();
        for _ in 0..10 {
            let (txid, _) = state.begin_tx();
            state.commit_tx(txid);
        }
        let mut page = vec![0u8; 64];
        state.persist_txid_counter(&mut page);

        let recovered = GlobalTxState::recover(&page);
        let (new_txid, _) = recovered.begin_tx();
        // Recovered TxIds must be greater than any pre-crash TxId.
        assert!(new_txid > state.global_xmax());
    }

    #[test]
    fn active_snapshot_has_no_owner() {
        let state = GlobalTxState::new();
        let (txid, _) = state.begin_tx();
        let snap = state.active_snapshot();
        // Active snapshot has owner=TX_ID_INVALID: it cannot see its own writes.
        assert_eq!(snap.owner_txid, TX_ID_INVALID);
        state.commit_tx(txid);
    }
}
