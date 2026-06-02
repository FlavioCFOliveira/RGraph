//! Sharded lock table with wait queues.
//!
//! [`LockTable`] provides per-resource locking for graph objects (nodes,
//! edges, properties).  Resources are identified by a `u64` key derived
//! from the physical address (page_id + slot_index) or from a logical ID.
//!
//! # Design
//!
//! * **1024 shards** — each shard is an independent `Mutex<HashMap>` so
//!   different resources rarely contend on the same shard lock.
//! * **Lock modes**: Shared (S), Exclusive (X), IntentionShared (IS),
//!   IntentionExclusive (IX).  Compatibility follows the standard 2PL matrix.
//! * **Wait queues** — blocked requesters are queued FIFO per lock entry
//!   and notified via a `crossbeam_channel::Sender<LockGranted>` when the
//!   lock becomes available.
//! * **No lock escalation** — IS/IX locks are never promoted to S/X.
//! * **Multiple holders** — S and IS locks allow multiple concurrent holders.
//!
//! # Lock compatibility matrix
//!
//! ```text
//!           IS   IX    S    X
//! IS         Y    Y    Y    N
//! IX         Y    Y    N    N
//! S          Y    N    Y    N
//! X          N    N    N    N
//! ```

use crate::txn::txid::TxId;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// The mode of a lock request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Intention shared — intends to acquire shared locks on sub-resources.
    IntentionShared,
    /// Intention exclusive — intends to acquire exclusive locks on sub-resources.
    IntentionExclusive,
    /// Shared — multiple transactions may hold S simultaneously.
    Shared,
    /// Exclusive — only one holder; incompatible with all other modes.
    Exclusive,
}

impl LockMode {
    /// Returns `true` if `self` and `other` can be held concurrently by
    /// different transactions.
    pub fn compatible_with(self, other: LockMode) -> bool {
        use LockMode::*;
        matches!(
            (self, other),
            (IntentionShared, IntentionShared)
                | (IntentionShared, IntentionExclusive)
                | (IntentionShared, Shared)
                | (IntentionExclusive, IntentionShared)
                | (IntentionExclusive, IntentionExclusive)
                | (Shared, IntentionShared)
                | (Shared, Shared)
        )
    }

    /// Returns `true` if this mode allows multiple concurrent holders.
    pub fn allows_multiple_holders(self) -> bool {
        use LockMode::*;
        matches!(self, IntentionShared | IntentionExclusive | Shared)
    }
}

/// Result of a lock acquisition attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockResult {
    /// Lock granted immediately.
    Granted,
    /// Lock cannot be granted — caller should trigger wound-wait and retry
    /// or wait on the receiver.
    Denied,
}

/// A single holder entry within a lock.
#[derive(Debug)]
struct Holder {
    txid: TxId,
    mode: LockMode,
}

/// Per-resource lock entry stored in a shard.
#[derive(Debug)]
struct LockEntry {
    /// Current holders (may be multiple for S/IS/IX modes).
    holders: Vec<Holder>,
    /// The effective lock mode currently held (highest-priority mode
    /// among holders, or the mode for which we track compatibility).
    /// For simplicity we track the raw list and recompute on release.
    /// FIFO wait queue: (txid, requested_mode, notification channel).
    waiters: VecDeque<(TxId, LockMode, Sender<LockResult>)>,
}

impl LockEntry {
    fn new() -> Self {
        Self {
            holders: Vec::new(),
            waiters: VecDeque::new(),
        }
    }

    /// Return `true` if `txid` already holds a lock of `mode` or stronger.
    fn already_holds(&self, txid: TxId, mode: LockMode) -> bool {
        self.holders
            .iter()
            .any(|h| h.txid == txid && self.mode_at_least_as_strong(h.mode, mode))
    }

    /// Return `true` if `held` is at least as strong as `required`.
    fn mode_at_least_as_strong(&self, held: LockMode, required: LockMode) -> bool {
        use LockMode::*;
        match required {
            IntentionShared => true, // any mode covers IS
            IntentionExclusive => {
                matches!(held, IntentionExclusive | Exclusive)
            }
            Shared => matches!(held, Shared | Exclusive),
            Exclusive => held == Exclusive,
        }
    }

    /// Return `true` if `mode` is compatible with all current holders
    /// (excluding `requesting_txid` itself, which may already hold a lock).
    fn compatible_with_holders(&self, requesting_txid: TxId, mode: LockMode) -> bool {
        for h in &self.holders {
            if h.txid == requesting_txid {
                continue; // a txid is always compatible with itself
            }
            if !mode.compatible_with(h.mode) {
                return false;
            }
        }
        true
    }

    /// Add a holder.
    fn add_holder(&mut self, txid: TxId, mode: LockMode) {
        self.holders.push(Holder { txid, mode });
    }

    /// Remove one holder entry for `txid` (the first match).
    fn remove_holder(&mut self, txid: TxId) {
        if let Some(pos) = self.holders.iter().position(|h| h.txid == txid) {
            self.holders.swap_remove(pos);
        }
    }

    /// Returns `true` if there are no holders remaining.
    fn is_empty(&self) -> bool {
        self.holders.is_empty()
    }

    /// Drain waiters that are now compatible with the current holder set,
    /// granting their locks in FIFO order.  Stops at the first incompatible
    /// waiter to preserve FIFO ordering.
    fn grant_compatible_waiters(&mut self) {
        while let Some((txid, mode, _)) = self.waiters.front() {
            let txid = *txid;
            let mode = *mode;
            if self.compatible_with_holders(txid, mode) {
                let (_, _, sender) = self.waiters.pop_front().unwrap();
                self.add_holder(txid, mode);
                // Ignore send errors — the waiter may have been aborted.
                let _ = sender.send(LockResult::Granted);
            } else {
                break;
            }
        }
    }
}

/// The number of shards in the lock table.
pub const N_SHARDS: usize = 1024;

/// Sharded lock table supporting shared, exclusive, and intention locks.
///
/// # Thread safety
///
/// `LockTable` is `Send + Sync`.  Each shard is independently locked so
/// operations on different resources proceed concurrently.
pub struct LockTable {
    /// Array of `N_SHARDS` shards.  Boxed to avoid stack overflow on
    /// initialisation.
    shards: Box<[Mutex<HashMap<u64, LockEntry>>]>,
}

impl LockTable {
    /// Create a new lock table with [`N_SHARDS`] shards.
    pub fn new() -> Self {
        let shards: Vec<_> = (0..N_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    /// Compute the shard index for `resource_id`.
    fn shard_index(resource_id: u64) -> usize {
        // Fibonacci hashing to spread sequential resource IDs across shards.
        let hash = resource_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (hash >> (64 - 10)) as usize // top 10 bits → 1024 shards
    }

    /// Attempt to acquire a lock on `resource_id` for `txid` in `mode`.
    ///
    /// Returns [`LockResult::Granted`] if the lock was granted immediately,
    /// or a receiver that will yield [`LockResult::Granted`] once granted.
    /// The caller is responsible for blocking on the receiver and for
    /// invoking wound-wait if `LockResult::Denied` is returned instead.
    ///
    /// If the transaction already holds a lock of `mode` or stronger, the
    /// call returns `Granted` immediately (idempotent).
    pub fn try_acquire(
        &self,
        resource_id: u64,
        txid: TxId,
        mode: LockMode,
    ) -> (LockResult, Option<Receiver<LockResult>>) {
        let idx = Self::shard_index(resource_id);
        let mut shard = self
            .shards[idx]
            .lock()
            .expect("INVARIANT: lock table shard mutex must not be poisoned");

        let entry = shard.entry(resource_id).or_insert_with(LockEntry::new);

        // Idempotency: already holds a compatible or stronger lock.
        if entry.already_holds(txid, mode) {
            return (LockResult::Granted, None);
        }

        if entry.compatible_with_holders(txid, mode) {
            entry.add_holder(txid, mode);
            (LockResult::Granted, None)
        } else {
            // Lock is contested — give the caller a channel to wait on.
            let (tx, rx) = bounded(1);
            entry.waiters.push_back((txid, mode, tx));
            (LockResult::Denied, Some(rx))
        }
    }

    /// Release `txid`'s lock on `resource_id` and wake compatible waiters.
    pub fn release(&self, resource_id: u64, txid: TxId) {
        let idx = Self::shard_index(resource_id);
        let mut shard = self.shards[idx]
            .lock()
            .expect("INVARIANT: lock table shard mutex must not be poisoned");

        if let Some(entry) = shard.get_mut(&resource_id) {
            entry.remove_holder(txid);
            entry.grant_compatible_waiters();
            if entry.is_empty() && entry.waiters.is_empty() {
                shard.remove(&resource_id);
            }
        }
    }

    /// Release all locks held by `txid` on the given `resources`.
    ///
    /// This is called during commit or abort to free all locks at once.
    /// The `resources` slice must be the complete set of resource IDs held
    /// by this transaction (typically stored in [`Transaction::held_locks`]).
    pub fn release_all(&self, txid: TxId, resources: &[u64]) {
        for &resource_id in resources {
            self.release(resource_id, txid);
        }
    }

    /// Returns the TxId of all current holders for `resource_id`.
    ///
    /// Primarily used by wound-wait to identify the transaction to wound.
    pub fn holders(&self, resource_id: u64) -> Vec<(TxId, LockMode)> {
        let idx = Self::shard_index(resource_id);
        let shard = self.shards[idx]
            .lock()
            .expect("INVARIANT: lock table shard mutex must not be poisoned");
        if let Some(entry) = shard.get(&resource_id) {
            entry
                .holders
                .iter()
                .map(|h| (h.txid, h.mode))
                .collect()
        } else {
            Vec::new()
        }
    }
}

impl Default for LockTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // LockMode compatibility matrix
    // ------------------------------------------------------------------

    #[test]
    fn is_compatible_with_is() {
        assert!(LockMode::IntentionShared.compatible_with(LockMode::IntentionShared));
    }

    #[test]
    fn is_compatible_with_ix() {
        assert!(LockMode::IntentionShared.compatible_with(LockMode::IntentionExclusive));
    }

    #[test]
    fn is_compatible_with_s() {
        assert!(LockMode::IntentionShared.compatible_with(LockMode::Shared));
    }

    #[test]
    fn is_incompatible_with_x() {
        assert!(!LockMode::IntentionShared.compatible_with(LockMode::Exclusive));
    }

    #[test]
    fn ix_incompatible_with_s() {
        assert!(!LockMode::IntentionExclusive.compatible_with(LockMode::Shared));
    }

    #[test]
    fn ix_incompatible_with_x() {
        assert!(!LockMode::IntentionExclusive.compatible_with(LockMode::Exclusive));
    }

    #[test]
    fn s_incompatible_with_ix() {
        assert!(!LockMode::Shared.compatible_with(LockMode::IntentionExclusive));
    }

    #[test]
    fn s_incompatible_with_x() {
        assert!(!LockMode::Shared.compatible_with(LockMode::Exclusive));
    }

    #[test]
    fn x_incompatible_with_everything() {
        for other in [
            LockMode::IntentionShared,
            LockMode::IntentionExclusive,
            LockMode::Shared,
            LockMode::Exclusive,
        ] {
            assert!(
                !LockMode::Exclusive.compatible_with(other),
                "Exclusive should be incompatible with {other:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Immediate grants
    // ------------------------------------------------------------------

    #[test]
    fn shared_locks_are_compatible() {
        let table = LockTable::new();
        let (r1, _) = table.try_acquire(1, 10, LockMode::Shared);
        let (r2, _) = table.try_acquire(1, 11, LockMode::Shared);
        assert_eq!(r1, LockResult::Granted);
        assert_eq!(r2, LockResult::Granted);
    }

    #[test]
    fn exclusive_lock_denies_shared() {
        let table = LockTable::new();
        let (r1, _) = table.try_acquire(1, 10, LockMode::Exclusive);
        assert_eq!(r1, LockResult::Granted);
        let (r2, _) = table.try_acquire(1, 11, LockMode::Shared);
        assert_eq!(r2, LockResult::Denied);
    }

    #[test]
    fn exclusive_lock_denies_exclusive() {
        let table = LockTable::new();
        let (r1, _) = table.try_acquire(1, 10, LockMode::Exclusive);
        assert_eq!(r1, LockResult::Granted);
        let (r2, _) = table.try_acquire(1, 11, LockMode::Exclusive);
        assert_eq!(r2, LockResult::Denied);
    }

    #[test]
    fn same_txid_is_idempotent() {
        let table = LockTable::new();
        let (r1, _) = table.try_acquire(1, 10, LockMode::Exclusive);
        let (r2, _) = table.try_acquire(1, 10, LockMode::Exclusive);
        assert_eq!(r1, LockResult::Granted);
        assert_eq!(r2, LockResult::Granted);
    }

    // ------------------------------------------------------------------
    // Release and wait-queue
    // ------------------------------------------------------------------

    #[test]
    fn release_wakes_waiter() {
        let table = LockTable::new();
        // tx 10 holds exclusive.
        let (_, _) = table.try_acquire(1, 10, LockMode::Exclusive);
        // tx 11 is denied and gets a receiver.
        let (r2, rx2) = table.try_acquire(1, 11, LockMode::Exclusive);
        assert_eq!(r2, LockResult::Denied);
        let rx = rx2.expect("should have a receiver");

        // Release tx 10 — tx 11 should be granted.
        table.release(1, 10);
        let result = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(result, LockResult::Granted);
    }

    #[test]
    fn release_all_frees_multiple_resources() {
        let table = LockTable::new();
        table.try_acquire(1, 10, LockMode::Exclusive);
        table.try_acquire(2, 10, LockMode::Exclusive);
        table.try_acquire(3, 10, LockMode::Shared);

        table.release_all(10, &[1, 2, 3]);

        // After release_all, other txns can acquire immediately.
        let (r1, _) = table.try_acquire(1, 11, LockMode::Exclusive);
        let (r2, _) = table.try_acquire(2, 11, LockMode::Exclusive);
        let (r3, _) = table.try_acquire(3, 11, LockMode::Exclusive);
        assert_eq!(r1, LockResult::Granted);
        assert_eq!(r2, LockResult::Granted);
        assert_eq!(r3, LockResult::Granted);
    }

    #[test]
    fn wait_queue_is_fifo() {
        let table = LockTable::new();
        // tx 10 holds exclusive.
        table.try_acquire(1, 10, LockMode::Exclusive);
        // tx 11 and tx 12 queue up.
        let (_, rx11_opt) = table.try_acquire(1, 11, LockMode::Exclusive);
        let (_, rx12_opt) = table.try_acquire(1, 12, LockMode::Exclusive);
        let rx11 = rx11_opt.unwrap();
        let rx12 = rx12_opt.unwrap();

        // Release tx 10 — tx 11 should get the lock (FIFO).
        table.release(1, 10);
        let r11 = rx11.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(r11, LockResult::Granted);

        // tx 12 must still be waiting.
        assert!(
            rx12.try_recv().is_err(),
            "tx12 should not be granted yet"
        );

        // Release tx 11 — now tx 12 gets the lock.
        table.release(1, 11);
        let r12 = rx12.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(r12, LockResult::Granted);
    }

    #[test]
    fn different_resources_do_not_interfere() {
        let table = LockTable::new();
        let (r1, _) = table.try_acquire(1, 10, LockMode::Exclusive);
        let (r2, _) = table.try_acquire(2, 11, LockMode::Exclusive);
        assert_eq!(r1, LockResult::Granted);
        assert_eq!(r2, LockResult::Granted);
    }

    #[test]
    fn holders_returns_current_holders() {
        let table = LockTable::new();
        table.try_acquire(5, 10, LockMode::Shared);
        table.try_acquire(5, 11, LockMode::Shared);

        let holders = table.holders(5);
        let txids: Vec<TxId> = holders.iter().map(|(t, _)| *t).collect();
        assert!(txids.contains(&10));
        assert!(txids.contains(&11));
    }

    #[test]
    fn entry_cleaned_up_after_all_released() {
        let table = LockTable::new();
        table.try_acquire(42, 10, LockMode::Exclusive);
        table.release(42, 10);
        // After release, holders returns empty (entry removed).
        assert!(table.holders(42).is_empty());
    }

    #[test]
    fn shared_locks_all_granted_after_exclusive_releases() {
        let table = LockTable::new();
        table.try_acquire(1, 10, LockMode::Exclusive);

        let (_, rx11_opt) = table.try_acquire(1, 11, LockMode::Shared);
        let (_, rx12_opt) = table.try_acquire(1, 12, LockMode::Shared);
        let rx11 = rx11_opt.unwrap();
        let rx12 = rx12_opt.unwrap();

        table.release(1, 10);

        // Both shared lockers should be granted (compatible with each other).
        let r11 = rx11.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let r12 = rx12.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(r11, LockResult::Granted);
        assert_eq!(r12, LockResult::Granted);
    }
}
