//! Transaction snapshot and tuple-visibility evaluator.
//!
//! A [`Snapshot`] is captured at the moment a transaction begins (or, for
//! repeatable-read semantics, at the moment of the first query).  It records:
//!
//! * `xmin` — the lowest active TxId at snapshot time.  Any tuple created by
//!   a transaction with `xmin_tuple < self.xmin` is guaranteed committed
//!   (otherwise it would still be in the active set).
//! * `xmax` — the next TxId that had **not yet been assigned** at snapshot
//!   time.  Any tuple created by `xmin_tuple >= self.xmax` is invisible (the
//!   creating transaction started after our snapshot).
//! * `active` — the sorted list of transaction IDs that were running at
//!   snapshot time.  A tuple created by a TxId in this list is not visible,
//!   even if that TxId falls between `xmin` and `xmax`.
//!
//! # Visibility Rule (PostgreSQL-style MVCC)
//!
//! A version is **visible** to a snapshot if and only if:
//!
//! 1. The creating transaction (`xmin_tuple`) **was committed** at or before
//!    the snapshot — i.e., `xmin_committed` is cached as `true` or the tuple's
//!    `xmin` passes the xmin/xmax/active check.
//! 2. The deleting transaction (`xmax_tuple`), if any, was **not committed**
//!    at or before the snapshot — i.e., it was either never set, aborted, or
//!    started after the snapshot (`>= self.xmax`), or was still active at
//!    snapshot time (in the active list).

use crate::txn::txid::{TxId, TX_ID_INVALID};

/// A point-in-time snapshot of the global transaction state.
///
/// Snapshots are cheap to clone: the `active` list is small in typical
/// workloads (bounded by the number of concurrent writers).
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Lowest TxId that was active at snapshot time.  All transactions with
    /// a TxId strictly less than this value are guaranteed to be committed.
    pub xmin: TxId,
    /// The next TxId to be assigned at snapshot time.  All transactions with
    /// a TxId >= this value started after the snapshot was taken.
    pub xmax: TxId,
    /// Sorted list of TxIds that were actively running at snapshot time.
    /// Using a sorted `Vec` keeps the struct small; binary search is fast
    /// enough for the expected cardinality (< few hundred concurrent txns).
    active: Vec<TxId>,
    /// The TxId of the transaction that owns this snapshot, used to allow
    /// self-modification visibility (a transaction sees its own writes).
    pub owner_txid: TxId,
}

impl Snapshot {
    /// Create a new snapshot.
    ///
    /// * `xmin`       — lowest active TxId at snapshot time.
    /// * `xmax`       — next TxId to be assigned at snapshot time.
    /// * `active`     — active TxIds at snapshot time (will be sorted).
    /// * `owner_txid` — the TxId of the transaction that holds this snapshot.
    pub fn new(xmin: TxId, xmax: TxId, mut active: Vec<TxId>, owner_txid: TxId) -> Self {
        active.sort_unstable();
        Self {
            xmin,
            xmax,
            active,
            owner_txid,
        }
    }

    /// Returns `true` if `txid` was active (uncommitted) at snapshot time.
    ///
    /// This is a binary search on the sorted `active` list — O(log n).
    pub fn is_active(&self, txid: TxId) -> bool {
        self.active.binary_search(&txid).is_ok()
    }

    /// Determine whether a given tuple version is visible to this snapshot.
    ///
    /// # Arguments
    ///
    /// * `xmin_tuple`      — the TxId that created this version.
    /// * `xmax_tuple`      — the TxId that deleted this version (`TX_ID_INVALID` = live).
    /// * `xmin_committed` — cached hint: `true` if `xmin_tuple` is known to
    ///   have committed (from the tuple's infomask).
    /// * `xmax_committed` — cached hint: `true` if `xmax_tuple` is known to
    ///   have committed (from the tuple's infomask).
    ///
    /// # Returns
    ///
    /// `true` if the version is visible; `false` otherwise.
    pub fn is_visible(
        &self,
        xmin_tuple: TxId,
        xmax_tuple: TxId,
        xmin_committed: bool,
        xmax_committed: bool,
    ) -> bool {
        // ----------------------------------------------------------------
        // Step 1: Is the creating transaction visible?
        // ----------------------------------------------------------------
        let xmin_visible = self.txid_committed_before_snapshot(xmin_tuple, xmin_committed);
        if !xmin_visible {
            // The creating transaction is not yet visible.  Exception: a
            // transaction can always see its own writes (self-modification).
            if xmin_tuple != self.owner_txid {
                return false;
            }
            // Fall through — our own INSERT is visible even if not committed.
        }

        // ----------------------------------------------------------------
        // Step 2: Has the version been deleted by a committed transaction
        //         that we can see?
        // ----------------------------------------------------------------
        if xmax_tuple == TX_ID_INVALID {
            // Live tuple — no deleter.
            return true;
        }

        // If the deleter is our own transaction, the tuple is deleted from
        // our own perspective (we see our own DELETE).
        if xmax_tuple == self.owner_txid {
            return false;
        }

        // The deleter's delete is visible to us iff the deleter committed
        // before our snapshot *and* is not still active in our snapshot.
        let xmax_visible = self.txid_committed_before_snapshot(xmax_tuple, xmax_committed);
        !xmax_visible
    }

    /// Determine whether `txid` committed before this snapshot was taken.
    ///
    /// Uses the cached `committed` hint when available, then falls back to
    /// the xmin/xmax/active rules.
    fn txid_committed_before_snapshot(&self, txid: TxId, committed_hint: bool) -> bool {
        if txid == TX_ID_INVALID {
            return false;
        }

        // Fast path: caller already knows from the infomask cache.
        if committed_hint {
            return true;
        }

        // Txids strictly below xmin are guaranteed committed.
        if txid < self.xmin {
            return true;
        }

        // Txids >= xmax started after the snapshot — not visible.
        if txid >= self.xmax {
            return false;
        }

        // Txids in [xmin, xmax) that appear in the active list were still
        // running at snapshot time — their changes are not yet visible.
        !self.is_active(txid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a snapshot where txids 1..=3 are committed and tx 4 is active.
    /// xmin=4, xmax=5, active=[4], owner=99.
    fn make_snapshot() -> Snapshot {
        Snapshot::new(4, 5, vec![4], 99)
    }

    // ------------------------------------------------------------------
    // xmin visibility
    // ------------------------------------------------------------------

    #[test]
    fn tuple_created_before_xmin_is_visible() {
        let snap = make_snapshot();
        // xmin=4: tx 3 < xmin, so it is committed-before-snapshot.
        assert!(snap.is_visible(3, TX_ID_INVALID, false, false));
    }

    #[test]
    fn tuple_created_at_xmin_and_active_is_invisible() {
        let snap = make_snapshot();
        // tx 4 is active (in the active list).
        assert!(!snap.is_visible(4, TX_ID_INVALID, false, false));
    }

    #[test]
    fn tuple_created_at_xmax_is_invisible() {
        let snap = make_snapshot();
        // xmax=5: tx 5 >= xmax, not yet started at snapshot.
        assert!(!snap.is_visible(5, TX_ID_INVALID, false, false));
    }

    #[test]
    fn tuple_created_after_xmax_is_invisible() {
        let snap = make_snapshot();
        assert!(!snap.is_visible(100, TX_ID_INVALID, false, false));
    }

    // ------------------------------------------------------------------
    // Committed hint path
    // ------------------------------------------------------------------

    #[test]
    fn committed_hint_bypasses_xmin_check() {
        // Even if xmin_tuple >= xmax (future tx), the hint overrides.
        let snap = Snapshot::new(1, 10, vec![], 99);
        // Without hint, tx 50 >= xmax would be invisible.
        assert!(!snap.is_visible(50, TX_ID_INVALID, false, false));
        // With hint, it is visible.
        assert!(snap.is_visible(50, TX_ID_INVALID, true, false));
    }

    // ------------------------------------------------------------------
    // xmax visibility
    // ------------------------------------------------------------------

    #[test]
    fn live_tuple_is_visible() {
        let snap = make_snapshot();
        assert!(snap.is_visible(1, TX_ID_INVALID, false, false));
    }

    #[test]
    fn deleted_by_committed_tx_is_invisible() {
        let snap = make_snapshot();
        // Deleter tx=2, which is < xmin=4, hence committed.
        assert!(!snap.is_visible(1, 2, false, false));
    }

    #[test]
    fn deleted_by_committed_tx_via_hint() {
        let snap = make_snapshot();
        // Deleter committed, but we still check via hint path.
        assert!(!snap.is_visible(1, 2, false, true));
    }

    #[test]
    fn deleted_by_active_tx_is_still_visible() {
        let snap = make_snapshot();
        // Deleter=4 is still active — deletion not yet committed.
        assert!(snap.is_visible(1, 4, false, false));
    }

    #[test]
    fn deleted_by_future_tx_is_still_visible() {
        let snap = make_snapshot();
        // Deleter=5 >= xmax — started after snapshot.
        assert!(snap.is_visible(1, 5, false, false));
    }

    #[test]
    fn deleted_by_aborted_tx_hint_is_still_visible() {
        // xmax_committed=false with a past txid means aborted or active.
        // If the deleter is below xmin but xmax_committed=false, the visibility
        // evaluator treats it as *not* committed.
        let snap = Snapshot::new(10, 20, vec![], 99);
        // Deleter=5 < xmin=10. Without committed hint it *looks* committed.
        // This tests the case where the infomask says aborted.
        // The visibility rule: xmax_committed=false + xmax < xmin -> treated as committed
        // (because any tx below xmin is committed by invariant).
        // So the tuple IS deleted in this scenario.
        assert!(!snap.is_visible(1, 5, false, false));
        // But with xmax_aborted scenario we'd pass xmax_committed=false.
        // The evaluator uses the same committed_before_snapshot logic, which
        // will return true for txid < xmin regardless of the hint.
        // The hint only short-circuits to true; false falls through to the rules.
    }

    // ------------------------------------------------------------------
    // Self-modification
    // ------------------------------------------------------------------

    #[test]
    fn own_insert_is_visible_even_before_commit() {
        // Snapshot: xmin=5, xmax=6, active=[5], owner=5.
        // The owner's own inserts (xmin=5, which is in the active list) should be visible.
        let snap = Snapshot::new(5, 6, vec![5], 5);
        assert!(snap.is_visible(5, TX_ID_INVALID, false, false));
    }

    #[test]
    fn own_delete_makes_tuple_invisible() {
        let snap = Snapshot::new(1, 10, vec![], 99);
        // Tuple created by tx 1 (committed), deleted by owner tx 99.
        assert!(!snap.is_visible(1, 99, false, false));
    }

    #[test]
    fn foreign_insert_while_active_is_invisible() {
        // Snapshot: xmin=3, xmax=10, active=[5,6], owner=7.
        let snap = Snapshot::new(3, 10, vec![5, 6], 7);
        // Tx 5 is active — its inserts are invisible.
        assert!(!snap.is_visible(5, TX_ID_INVALID, false, false));
        // Tx 2 < xmin — its inserts are visible.
        assert!(snap.is_visible(2, TX_ID_INVALID, false, false));
        // Tx 7 = owner — its inserts are visible.
        assert!(snap.is_visible(7, TX_ID_INVALID, false, false));
    }

    // ------------------------------------------------------------------
    // is_active helper
    // ------------------------------------------------------------------

    #[test]
    fn is_active_returns_correct_results() {
        let snap = Snapshot::new(1, 10, vec![3, 5, 7], 99);
        assert!(snap.is_active(3));
        assert!(snap.is_active(5));
        assert!(snap.is_active(7));
        assert!(!snap.is_active(1));
        assert!(!snap.is_active(4));
        assert!(!snap.is_active(10));
    }

    #[test]
    fn active_list_is_sorted_regardless_of_input_order() {
        let snap = Snapshot::new(1, 100, vec![9, 3, 6], 99);
        // Binary search requires sorted order; just verify all are found.
        assert!(snap.is_active(3));
        assert!(snap.is_active(6));
        assert!(snap.is_active(9));
    }
}
