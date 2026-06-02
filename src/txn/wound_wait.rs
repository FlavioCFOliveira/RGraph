//! Wound-wait deadlock prevention.
//!
//! The wound-wait protocol prevents deadlocks in a distributed locking
//! scenario by comparing the *ages* of transactions (represented by their
//! TxIds — a lower TxId means an older transaction):
//!
//! * If the **requester** is **older** than the **holder** (requester.txid < holder.txid),
//!   the holder is aborted ("wounded") to unblock the older requester.
//! * If the **requester** is **younger** than the **holder** (requester.txid > holder.txid),
//!   the requester waits.
//!
//! This guarantees freedom from deadlock because the partial order of
//! transaction ages is acyclic: a younger transaction always waits for an
//! older one and is never granted priority over it.
//!
//! # Starvation prevention
//!
//! A transaction that has been wounded three or more times is granted
//! *immunity*: it will no longer be wounded by future conflict checks.
//! Once immune, conflicting younger requesters must wait instead of wounding
//! it.  Immunity is revoked when the transaction commits or aborts
//! (via [`WoundWait::cleanup`]).

use crate::txn::txid::TxId;
use std::collections::HashMap;
use std::sync::Mutex;

/// The action recommended by the wound-wait check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WoundAction {
    /// The holder should be aborted (wound it).
    Wound,
    /// The requester should block and wait.
    Wait,
}

/// Wound-wait deadlock prevention oracle.
///
/// Thread-safe: a single instance is shared across all transactions.
pub struct WoundWait {
    /// Number of times each active transaction has been wounded.
    /// Entries are created on first wound and removed on cleanup.
    inner: Mutex<HashMap<TxId, u8>>,
}

/// After this many wounds, a transaction gains immunity.
const IMMUNITY_THRESHOLD: u8 = 3;

impl WoundWait {
    /// Create a new wound-wait oracle.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Determine the action for a lock conflict between `requester` and `holder`.
    ///
    /// `requester_ts` and `holder_ts` are the TxIds (which encode age:
    /// lower = older).
    ///
    /// # Returns
    ///
    /// * [`WoundAction::Wound`] — the caller must abort the holder transaction.
    /// * [`WoundAction::Wait`] — the requester must block until the holder
    ///   releases the lock.
    ///
    /// Immunity is checked for `holder_ts`: if the holder is immune, the
    /// requester always waits regardless of age ordering.
    pub fn check(
        &self,
        requester_ts: TxId,
        holder_ts: TxId,
    ) -> WoundAction {
        // An older requester wounds a younger holder — unless the holder is
        // immune.
        if requester_ts < holder_ts {
            if self.is_immune(holder_ts) {
                WoundAction::Wait
            } else {
                WoundAction::Wound
            }
        } else {
            // Requester is younger or the same age — always wait.
            WoundAction::Wait
        }
    }

    /// Record that `victim` has been wounded.
    ///
    /// Increments the wound counter for `victim`.  Once the counter reaches
    /// [`IMMUNITY_THRESHOLD`], subsequent calls to [`WoundWait::check`] will
    /// return [`WoundAction::Wait`] for conflicts involving `victim` as the
    /// holder (i.e., the victim is no longer woundable).
    ///
    /// The counter is saturating: it will not overflow.
    pub fn record_wound(&self, victim: TxId) {
        let mut guard = self
            .inner
            .lock()
            .expect("INVARIANT: wound_wait mutex must not be poisoned");
        let count = guard.entry(victim).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Returns `true` if `txid` has accumulated enough wounds to be immune.
    pub fn is_immune(&self, txid: TxId) -> bool {
        let guard = self
            .inner
            .lock()
            .expect("INVARIANT: wound_wait mutex must not be poisoned");
        guard.get(&txid).copied().unwrap_or(0) >= IMMUNITY_THRESHOLD
    }

    /// Remove the wound counter for `txid`.
    ///
    /// Must be called when a transaction commits or aborts.
    pub fn cleanup(&self, txid: TxId) {
        let mut guard = self
            .inner
            .lock()
            .expect("INVARIANT: wound_wait mutex must not be poisoned");
        guard.remove(&txid);
    }

    /// Return the current wound count for `txid` (0 if never wounded).
    pub fn wound_count(&self, txid: TxId) -> u8 {
        let guard = self
            .inner
            .lock()
            .expect("INVARIANT: wound_wait mutex must not be poisoned");
        guard.get(&txid).copied().unwrap_or(0)
    }
}

impl Default for WoundWait {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_wounds_younger() {
        let ww = WoundWait::new();
        // tx 1 (older) vs tx 5 (younger): wound.
        assert_eq!(ww.check(1, 5), WoundAction::Wound);
    }

    #[test]
    fn younger_waits_for_older() {
        let ww = WoundWait::new();
        // tx 5 (younger) vs tx 1 (older): wait.
        assert_eq!(ww.check(5, 1), WoundAction::Wait);
    }

    #[test]
    fn equal_age_waits() {
        let ww = WoundWait::new();
        assert_eq!(ww.check(3, 3), WoundAction::Wait);
    }

    #[test]
    fn wound_count_starts_at_zero() {
        let ww = WoundWait::new();
        assert_eq!(ww.wound_count(42), 0);
    }

    #[test]
    fn record_wound_increments_count() {
        let ww = WoundWait::new();
        ww.record_wound(10);
        assert_eq!(ww.wound_count(10), 1);
        ww.record_wound(10);
        assert_eq!(ww.wound_count(10), 2);
    }

    #[test]
    fn immunity_after_three_wounds() {
        let ww = WoundWait::new();
        let victim = 7u64;

        assert!(!ww.is_immune(victim));
        ww.record_wound(victim);
        assert!(!ww.is_immune(victim));
        ww.record_wound(victim);
        assert!(!ww.is_immune(victim));
        ww.record_wound(victim);
        assert!(ww.is_immune(victim));
    }

    #[test]
    fn immune_holder_is_not_wounded() {
        let ww = WoundWait::new();
        let victim = 20u64;
        // Make victim immune.
        for _ in 0..3 {
            ww.record_wound(victim);
        }
        assert!(ww.is_immune(victim));
        // Even though requester (tx 1) is older, victim is immune.
        assert_eq!(ww.check(1, victim), WoundAction::Wait);
    }

    #[test]
    fn cleanup_removes_wound_record() {
        let ww = WoundWait::new();
        ww.record_wound(5);
        ww.record_wound(5);
        ww.cleanup(5);
        assert_eq!(ww.wound_count(5), 0);
        assert!(!ww.is_immune(5));
    }

    #[test]
    fn cleanup_of_unknown_txid_is_noop() {
        let ww = WoundWait::new();
        ww.cleanup(999); // Should not panic.
    }

    #[test]
    fn wound_count_is_saturating() {
        let ww = WoundWait::new();
        // Saturate at u8::MAX.
        for _ in 0..=300u32 {
            ww.record_wound(1);
        }
        assert_eq!(ww.wound_count(1), u8::MAX);
        assert!(ww.is_immune(1));
    }

    #[test]
    fn multiple_txids_are_tracked_independently() {
        let ww = WoundWait::new();
        ww.record_wound(1);
        ww.record_wound(1);
        ww.record_wound(2);

        assert_eq!(ww.wound_count(1), 2);
        assert_eq!(ww.wound_count(2), 1);
        assert!(!ww.is_immune(1));
        assert!(!ww.is_immune(2));

        ww.record_wound(1);
        assert!(ww.is_immune(1));
        assert!(!ww.is_immune(2));
    }

    #[test]
    fn cleanup_revokes_immunity() {
        let ww = WoundWait::new();
        for _ in 0..3 {
            ww.record_wound(42);
        }
        assert!(ww.is_immune(42));
        ww.cleanup(42);
        // After cleanup, immunity is revoked (counter reset to 0).
        assert!(!ww.is_immune(42));
        // And the older-wounds-younger rule applies again.
        assert_eq!(ww.check(1, 42), WoundAction::Wound);
    }
}
