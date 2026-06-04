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
//! *immunity*: it will no longer be wounded by **younger** requesters.
//! An **older** requester always wounds regardless of the holder's immunity —
//! otherwise the age ordering invariant (which guarantees deadlock freedom)
//! would be broken.  Immunity is revoked when the transaction commits or
//! aborts (via [`WoundWait::cleanup`]).
//!
//! # Victim abort callback
//!
//! When [`WoundWait::check`] returns [`WoundAction::Wound`], the engine layer
//! is responsible for actually aborting the victim.  [`WoundWait::record_wound`]
//! now accepts a callback `on_abort` that is called with the victim's TxId
//! before the wound counter is incremented, allowing the manager to set the
//! victim's status to `Aborted` and release its locks atomically.

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

/// After this many wounds, a transaction gains immunity against **younger**
/// requesters.  Older requesters (lower TxId) bypass immunity unconditionally
/// to preserve the age-ordering invariant.
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
    /// Immunity only shields the holder from wounding by *younger* requesters
    /// (requester_ts > holder_ts).  An older requester (requester_ts <
    /// holder_ts) always wounds, regardless of immunity, to preserve the
    /// age-ordering invariant that guarantees deadlock freedom.
    pub fn check(
        &self,
        requester_ts: TxId,
        holder_ts: TxId,
    ) -> WoundAction {
        if requester_ts < holder_ts {
            // Older requester vs. younger holder.
            // Immunity does NOT apply here: an older transaction must always
            // be able to wound a younger one, otherwise the age-ordering
            // invariant that guarantees deadlock freedom is broken.
            WoundAction::Wound
        } else {
            // Requester is younger or the same age — always wait.
            // Immunity is irrelevant in this branch (we already wait).
            WoundAction::Wait
        }
    }

    /// Record that `victim` has been wounded and invoke the abort callback.
    ///
    /// The `on_abort` callback is called with `victim` before the wound
    /// counter is incremented.  The engine layer uses this callback to
    /// atomically set the victim transaction's status to `Aborted` and
    /// release its locks.
    ///
    /// Once the counter reaches [`IMMUNITY_THRESHOLD`], `victim` can no
    /// longer be wounded by younger requesters (but older requesters still
    /// wound unconditionally — see [`WoundWait::check`]).
    ///
    /// The counter is saturating: it will not overflow past `u8::MAX`.
    pub fn record_wound(&self, victim: TxId, on_abort: impl FnOnce(TxId)) {
        // Invoke the engine-layer abort callback before touching the counter so
        // the victim is visible as Aborted as soon as possible.
        on_abort(victim);

        let mut guard = self
            .inner
            .lock()
            .expect("INVARIANT: wound_wait mutex must not be poisoned");
        let count = guard.entry(victim).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Returns `true` if `txid` has been wounded enough times to be immune
    /// against **younger** requesters.
    ///
    /// Immunity does NOT prevent an older transaction from wounding `txid`.
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
    use std::sync::{Arc, Mutex};

    /// Helper: call `record_wound` with a no-op abort callback.
    fn wound_noop(ww: &WoundWait, victim: TxId) {
        ww.record_wound(victim, |_| {});
    }

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
        wound_noop(&ww, 10);
        assert_eq!(ww.wound_count(10), 1);
        wound_noop(&ww, 10);
        assert_eq!(ww.wound_count(10), 2);
    }

    #[test]
    fn immunity_after_three_wounds() {
        let ww = WoundWait::new();
        let victim = 7u64;

        assert!(!ww.is_immune(victim));
        wound_noop(&ww, victim);
        assert!(!ww.is_immune(victim));
        wound_noop(&ww, victim);
        assert!(!ww.is_immune(victim));
        wound_noop(&ww, victim);
        assert!(ww.is_immune(victim));
    }

    /// Immunity protects a holder only against **younger** requesters —
    /// an older requester always wounds regardless of immunity, to
    /// preserve the age-ordering invariant.
    #[test]
    fn immune_holder_is_still_wounded_by_older_requester() {
        let ww = WoundWait::new();
        // victim = tx 20.  Make it immune (wound 3 times).
        let victim: TxId = 20;
        for _ in 0..3 {
            wound_noop(&ww, victim);
        }
        assert!(ww.is_immune(victim));

        // requester tx 1 is *older* (lower TxId) than victim tx 20.
        // An older requester must always wound — immunity must not invert
        // the age-ordering invariant.
        assert_eq!(ww.check(1, victim), WoundAction::Wound,
            "older requester must wound even an immune younger holder");
    }

    /// Immunity protects against a younger requester.
    #[test]
    fn immune_holder_is_not_wounded_by_younger_requester() {
        let ww = WoundWait::new();
        // victim = tx 5.  Make it immune.
        let victim: TxId = 5;
        for _ in 0..3 {
            wound_noop(&ww, victim);
        }
        assert!(ww.is_immune(victim));

        // requester tx 20 is *younger* (higher TxId) than victim tx 5.
        // The requester should wait regardless of immunity rules because
        // the age-ordering already requires a younger requester to wait.
        assert_eq!(ww.check(20, victim), WoundAction::Wait,
            "younger requester must wait for immune (or non-immune) older holder");
    }

    #[test]
    fn record_wound_invokes_abort_callback() {
        let ww = WoundWait::new();
        let callback_fired = Arc::new(Mutex::new(false));
        let fired_clone = Arc::clone(&callback_fired);

        ww.record_wound(42, move |_victim| {
            *fired_clone.lock().unwrap() = true;
        });

        assert!(*callback_fired.lock().unwrap(),
            "abort callback must be invoked by record_wound");
        assert_eq!(ww.wound_count(42), 1);
    }

    #[test]
    fn cleanup_removes_wound_record() {
        let ww = WoundWait::new();
        wound_noop(&ww, 5);
        wound_noop(&ww, 5);
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
            wound_noop(&ww, 1);
        }
        assert_eq!(ww.wound_count(1), u8::MAX);
        assert!(ww.is_immune(1));
    }

    #[test]
    fn multiple_txids_are_tracked_independently() {
        let ww = WoundWait::new();
        wound_noop(&ww, 1);
        wound_noop(&ww, 1);
        wound_noop(&ww, 2);

        assert_eq!(ww.wound_count(1), 2);
        assert_eq!(ww.wound_count(2), 1);
        assert!(!ww.is_immune(1));
        assert!(!ww.is_immune(2));

        wound_noop(&ww, 1);
        assert!(ww.is_immune(1));
        assert!(!ww.is_immune(2));
    }

    #[test]
    fn cleanup_revokes_immunity() {
        let ww = WoundWait::new();
        for _ in 0..3 {
            wound_noop(&ww, 42);
        }
        assert!(ww.is_immune(42));
        ww.cleanup(42);
        // After cleanup, immunity is revoked (counter reset to 0).
        assert!(!ww.is_immune(42));
        // And the older-wounds-younger rule applies again.
        assert_eq!(ww.check(1, 42), WoundAction::Wound);
    }
}
