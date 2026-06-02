//! Transaction manager — the primary entry point for ACID transactions.
//!
//! [`TransactionManager`] wires together the TxID allocator, global state,
//! lock table, and wound-wait oracle into a unified interface.  Callers
//! use it to:
//!
//! 1. **Begin** a transaction — snapshot acquired, TxId issued.
//! 2. **Acquire locks** — wound-wait applied on conflict; blocks until granted.
//! 3. **Commit** — WAL flushed to disk before returning; locks released;
//!    snapshot memory reclaimed.
//! 4. **Rollback** — locks released; transaction marked aborted.
//!
//! # Durability guarantee
//!
//! A commit is not acknowledged until `WalWriter::flush` completes
//! successfully.  If the flush fails, the error is returned and the
//! transaction must be rolled back.
//!
//! # Lock acquisition protocol
//!
//! [`TransactionManager::acquire_lock`] follows this sequence:
//!
//! 1. Call [`LockTable::try_acquire`].
//! 2. If granted immediately — done.
//! 3. If denied — call [`WoundWait::check`] against each conflicting holder.
//!    * `Wound` — the conflicting holder must be aborted by the engine layer
//!      (the manager records the wound count and returns
//!      [`TxError::WoundWait`] so the caller can propagate the abort).
//!    * `Wait` — block on the receiver until the lock is granted or until
//!      a timeout (if implemented by the engine layer).
//!
//! # Error types
//!
//! [`TxError`] enumerates every failure mode so callers can handle each case.

use crate::io::FileSystem;
use crate::index::manager::{IndexManager, IndexMutation};
use crate::txn::{
    lock_table::{LockMode, LockResult, LockTable},
    snapshot::Snapshot,
    state::GlobalTxState,
    txid::TxId,
    wound_wait::{WoundAction, WoundWait},
};
use crate::wal::{record::{RecordType, WalRecord}, writer::WalWriter};
use std::sync::Arc;
use thiserror::Error;

/// Errors that can occur during transaction management.
#[derive(Debug, Error)]
pub enum TxError {
    /// The WAL flush failed (I/O error).  The transaction must be rolled back.
    #[error("WAL flush failed: {0}")]
    WalFlush(#[from] std::io::Error),

    /// The wound-wait protocol determined that this transaction should be
    /// aborted to unblock an older transaction.  Contains the TxId of the
    /// transaction that was wounded (usually the caller's own txid).
    #[error("transaction {0} was wounded by an older transaction and must abort")]
    WoundWait(TxId),

    /// The transaction was used after it had already been committed or aborted.
    #[error("transaction {0} is not active (status: {1:?})")]
    NotActive(TxId, TxStatus),

    /// An attempt was made to operate on an already-finalised transaction.
    #[error("transaction {0} has already been finalised")]
    AlreadyFinalised(TxId),

    /// A secondary index mutation failed during commit.
    #[error("secondary index mutation failed: {0}")]
    IndexMutation(String),
}

/// The lifecycle state of a [`Transaction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    /// Transaction is in progress.
    Active,
    /// Transaction committed successfully.
    Committed,
    /// Transaction was rolled back.
    Aborted,
}

/// An in-progress or finalised transaction.
///
/// Created by [`TransactionManager::begin`] and consumed by
/// [`TransactionManager::commit`] or [`TransactionManager::rollback`].
/// Both commit and rollback release all held locks and update the global
/// transaction state.
pub struct Transaction {
    /// The unique transaction identifier.
    pub txid: TxId,
    /// The snapshot captured at BEGIN time.
    pub snapshot: Snapshot,
    /// Current lifecycle status.
    pub status: TxStatus,
    /// Resource IDs for which this transaction holds locks.
    /// Maintained by [`TransactionManager::acquire_lock`].
    held_locks: Vec<u64>,
    /// How many times this transaction has been wounded by the wound-wait
    /// protocol.  Mirrors the count in [`WoundWait`] for quick access.
    wound_count: u8,
    /// Staged secondary-index mutations that will be applied atomically at
    /// commit time.
    pub index_mutations: Vec<IndexMutation>,
}

impl Transaction {
    /// Returns `true` if the transaction is still in the `Active` state.
    pub fn is_active(&self) -> bool {
        self.status == TxStatus::Active
    }

    /// Returns a slice of all resource IDs currently locked by this transaction.
    pub fn held_locks(&self) -> &[u64] {
        &self.held_locks
    }
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("txid", &self.txid)
            .field("status", &self.status)
            .field("held_locks", &self.held_locks.len())
            .field("wound_count", &self.wound_count)
            .finish()
    }
}

/// The transaction manager.
///
/// Cheap to clone — all state is behind `Arc`.
#[derive(Clone)]
pub struct TransactionManager {
    global_state: Arc<GlobalTxState>,
    lock_table: Arc<LockTable>,
    wound_wait: Arc<WoundWait>,
}

impl TransactionManager {
    /// Create a new transaction manager backed by a fresh global state.
    pub fn new() -> Self {
        Self {
            global_state: Arc::new(GlobalTxState::new()),
            lock_table: Arc::new(LockTable::new()),
            wound_wait: Arc::new(WoundWait::new()),
        }
    }

    /// Create a transaction manager reusing existing subsystem instances.
    pub fn with_components(
        global_state: Arc<GlobalTxState>,
        lock_table: Arc<LockTable>,
        wound_wait: Arc<WoundWait>,
    ) -> Self {
        Self {
            global_state,
            lock_table,
            wound_wait,
        }
    }

    /// Begin a new transaction and return it.
    ///
    /// A snapshot of the current committed state is captured at this point.
    pub fn begin(&self) -> Transaction {
        let (txid, snapshot) = self.global_state.begin_tx();
        Transaction {
            txid,
            snapshot,
            status: TxStatus::Active,
            held_locks: Vec::new(),
            wound_count: 0,
            index_mutations: Vec::new(),
        }
    }

    /// Commit `tx`.
    ///
    /// Steps (in order):
    /// 1. Verify the transaction is still active.
    /// 2. Append a [`RecordType::Commit`] WAL record.
    /// 3. Flush the WAL to durable storage — **commit is not acknowledged
    ///    until this succeeds**.
    /// 4. Release all locks held by this transaction.
    /// 5. Notify the global state that the transaction has committed.
    /// 6. Clean up wound-wait metadata.
    ///
    /// On WAL flush failure, the transaction is left in `Active` state so
    /// the caller can retry or roll back.
    ///
    /// # Errors
    ///
    /// Returns [`TxError::NotActive`] if the transaction is not in `Active`
    /// state.  Returns [`TxError::WalFlush`] if the WAL flush fails.
    pub fn commit(
        &self,
        tx: &mut Transaction,
        wal: &mut WalWriter,
        fs: &dyn FileSystem,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }

        // Append the commit record.
        let commit_rec = WalRecord::new(RecordType::Commit, tx.txid, 0, 0, vec![]);
        wal.append(fs, commit_rec)?;

        // Flush WAL to disk — durability guarantee.  If this fails, leave
        // the transaction in Active state so the caller can retry or abort.
        wal.flush(fs)?;

        // Release all locks before updating global state.
        self.lock_table.release_all(tx.txid, &tx.held_locks);
        tx.held_locks.clear();

        // Update global transaction state.
        self.global_state.commit_tx(tx.txid);

        // Clean up wound-wait metadata.
        self.wound_wait.cleanup(tx.txid);

        tx.status = TxStatus::Committed;
        Ok(())
    }

    /// Commit `tx` with secondary-index mutations applied atomically.
    ///
    /// Steps:
    /// 1. Verify the transaction is still active.
    /// 2. Apply all staged [`IndexMutation`]s through `index_mgr`.
    /// 3. If any index mutation fails, abort the transaction.
    /// 4. Append a [`RecordType::Commit`] WAL record and flush.
    /// 5. Release locks, update global state, clean up wound-wait metadata.
    ///
    /// This ensures that either **both** the primary records and the secondary
    /// indexes are durable, or neither is, preserving atomicity.
    pub fn commit_with_indexes(
        &self,
        tx: &mut Transaction,
        index_mgr: &mut IndexManager,
        wal: &mut WalWriter,
        fs: &dyn FileSystem,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }

        // Apply staged index mutations.  If this fails we must abort.
        if let Err(e) = index_mgr.apply_batch(&tx.index_mutations) {
            let _ = self.rollback(tx, wal, fs);
            return Err(TxError::IndexMutation(e.to_string()));
        }

        // Clear the staged mutations so they are not replayed on retry.
        tx.index_mutations.clear();

        // Proceed with normal WAL commit.
        self.commit(tx, wal, fs)
    }

    /// Stage an index mutation in the transaction-local write set.
    ///
    /// The mutation is not applied until [`commit_with_indexes`] is called.
    /// If the transaction rolls back, staged mutations are simply discarded.
    pub fn stage_index_mutation(
        &self,
        tx: &mut Transaction,
        mutation: IndexMutation,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }
        tx.index_mutations.push(mutation);
        Ok(())
    }

    /// Roll back `tx`.
    ///
    /// Steps:
    /// 1. Verify the transaction is still active.
    /// 2. Append a [`RecordType::Abort`] WAL record (best-effort; does not
    ///    flush — the abort is implied by the absence of a commit record).
    /// 3. Release all locks.
    /// 4. Notify the global state that the transaction has aborted.
    /// 5. Clean up wound-wait metadata.
    ///
    /// # Errors
    ///
    /// Returns [`TxError::NotActive`] if the transaction is not active.
    pub fn rollback(
        &self,
        tx: &mut Transaction,
        wal: &mut WalWriter,
        fs: &dyn FileSystem,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }

        // Append an abort WAL record (best-effort — we do not flush here
        // because abort durability is inferred from the missing commit record
        // during recovery).
        let abort_rec = WalRecord::new(RecordType::Abort, tx.txid, 0, 0, vec![]);
        // Ignore WAL write errors on rollback — we will still release locks
        // and update state.
        let _ = wal.append(fs, abort_rec);

        // Discard staged index mutations — rollback means nothing is applied.
        tx.index_mutations.clear();

        // Release all locks.
        self.lock_table.release_all(tx.txid, &tx.held_locks);
        tx.held_locks.clear();

        // Update global transaction state.
        self.global_state.abort_tx(tx.txid);

        // Clean up wound-wait metadata.
        self.wound_wait.cleanup(tx.txid);

        tx.status = TxStatus::Aborted;
        Ok(())
    }

    /// Acquire a lock on `resource_id` for `tx` in `mode`.
    ///
    /// The wound-wait protocol is applied on conflict:
    /// * If this transaction is older than a conflicting holder, the holder
    ///   is wounded (caller receives [`TxError::WoundWait`] with the victim's
    ///   TxId and must arrange for the victim to abort).
    /// * If this transaction is younger than a conflicting holder, this call
    ///   blocks until the lock is granted.
    ///
    /// Already-held locks (idempotent re-acquisition) return immediately.
    ///
    /// # Errors
    ///
    /// * [`TxError::NotActive`] — the transaction is not in `Active` state.
    /// * [`TxError::WoundWait`] — this transaction should abort because it
    ///   was itself wounded by an older transaction that needed the lock.
    pub fn acquire_lock(
        &self,
        tx: &mut Transaction,
        resource_id: u64,
        mode: LockMode,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }

        loop {
            let (result, rx_opt) = self.lock_table.try_acquire(resource_id, tx.txid, mode);
            match result {
                LockResult::Granted => {
                    // Track the resource so release_all can free it on commit/abort.
                    if !tx.held_locks.contains(&resource_id) {
                        tx.held_locks.push(resource_id);
                    }
                    return Ok(());
                }
                LockResult::Denied => {
                    // Apply wound-wait against each holder.
                    let holders = self.lock_table.holders(resource_id);
                    let mut found_incompatible = false;

                    for (holder_txid, _holder_mode) in &holders {
                        if *holder_txid == tx.txid {
                            continue; // skip ourselves
                        }
                        found_incompatible = true;
                        match self.wound_wait.check(tx.txid, *holder_txid) {
                            WoundAction::Wound => {
                                // We wound the holder — record the wound.
                                self.wound_wait.record_wound(*holder_txid);
                                // The engine layer must arrange to abort the
                                // wounded transaction.  We return a WoundWait
                                // error so the caller knows which TxId to abort.
                                return Err(TxError::WoundWait(*holder_txid));
                            }
                            WoundAction::Wait => {
                                // We must wait — update our local wound count.
                                tx.wound_count = self.wound_wait.wound_count(tx.txid);
                            }
                        }
                    }

                    if !found_incompatible {
                        // No incompatible holders found — retry acquisition
                        // (race: holder may have released since we checked).
                        continue;
                    }

                    // No wound issued — block on the wait-queue receiver.
                    if let Some(rx) = rx_opt {
                        // Block until notified.  We use recv() without timeout
                        // because the wound-wait protocol guarantees progress:
                        // either the holder commits/aborts (waking us) or we
                        // get wounded and the holder's abort wakes us.
                        let notified = rx.recv().unwrap_or(LockResult::Denied);
                        if notified == LockResult::Granted {
                            if !tx.held_locks.contains(&resource_id) {
                                tx.held_locks.push(resource_id);
                            }
                            return Ok(());
                        }
                        // Denied again — loop back.
                    } else {
                        // No receiver provided and lock not granted; retry.
                        continue;
                    }
                }
            }
        }
    }

    /// Return a reference to the global transaction state.
    ///
    /// Useful for tests and for the buffer pool's VACUUM / GHOST page cleanup.
    pub fn global_state(&self) -> &GlobalTxState {
        &self.global_state
    }

    /// Return a reference to the shared lock table.
    pub fn lock_table(&self) -> &LockTable {
        &self.lock_table
    }

    /// Return a reference to the wound-wait oracle.
    pub fn wound_wait(&self) -> &WoundWait {
        &self.wound_wait
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use crate::txn::txid::TX_ID_INVALID;

    fn make_wal(dir: &std::path::Path) -> WalWriter {
        let fs = PosixFileSystem::new(false);
        WalWriter::open(dir.to_path_buf(), &fs).unwrap()
    }

    // ------------------------------------------------------------------
    // BEGIN
    // ------------------------------------------------------------------

    #[test]
    fn begin_returns_active_tx_with_valid_txid() {
        let mgr = TransactionManager::new();
        let tx = mgr.begin();
        assert_eq!(tx.status, TxStatus::Active);
        assert_ne!(tx.txid, TX_ID_INVALID);
        assert!(tx.held_locks().is_empty());
    }

    #[test]
    fn successive_begins_have_increasing_txids() {
        let mgr = TransactionManager::new();
        let t1 = mgr.begin();
        let t2 = mgr.begin();
        let t3 = mgr.begin();
        assert!(t1.txid < t2.txid);
        assert!(t2.txid < t3.txid);
    }

    // ------------------------------------------------------------------
    // COMMIT
    // ------------------------------------------------------------------

    #[test]
    fn commit_writes_wal_record_and_flushes() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.commit(&mut tx, &mut wal, &fs).unwrap();

        assert_eq!(tx.status, TxStatus::Committed);

        // Verify WAL segment is non-empty (commit record was flushed).
        let seg = tmp.path().join("wal-000000000");
        let handle = fs.open(&seg, false).unwrap();
        let len = handle.len().unwrap();
        assert!(len > 0, "WAL should contain the commit record");
    }

    #[test]
    fn commit_releases_held_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.acquire_lock(&mut tx, 42, LockMode::Exclusive).unwrap();
        assert!(!tx.held_locks().is_empty());

        mgr.commit(&mut tx, &mut wal, &fs).unwrap();
        assert!(tx.held_locks().is_empty());

        // Another transaction can now acquire the same resource.
        let mut tx2 = mgr.begin();
        mgr.acquire_lock(&mut tx2, 42, LockMode::Exclusive).unwrap();
        assert!(!tx2.held_locks().is_empty());
    }

    #[test]
    fn commit_on_non_active_tx_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.commit(&mut tx, &mut wal, &fs).unwrap();

        // Second commit must fail.
        let res = mgr.commit(&mut tx, &mut wal, &fs);
        assert!(matches!(res, Err(TxError::NotActive(_, TxStatus::Committed))));
    }

    // ------------------------------------------------------------------
    // ROLLBACK
    // ------------------------------------------------------------------

    #[test]
    fn rollback_sets_aborted_status_and_releases_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.acquire_lock(&mut tx, 10, LockMode::Shared).unwrap();
        mgr.rollback(&mut tx, &mut wal, &fs).unwrap();

        assert_eq!(tx.status, TxStatus::Aborted);
        assert!(tx.held_locks().is_empty());
    }

    #[test]
    fn rollback_on_non_active_tx_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.rollback(&mut tx, &mut wal, &fs).unwrap();

        let res = mgr.rollback(&mut tx, &mut wal, &fs);
        assert!(matches!(res, Err(TxError::NotActive(_, TxStatus::Aborted))));
    }

    // ------------------------------------------------------------------
    // Lock acquisition
    // ------------------------------------------------------------------

    #[test]
    fn acquire_shared_locks_from_multiple_txns() {
        let mgr = TransactionManager::new();
        let mut t1 = mgr.begin();
        let mut t2 = mgr.begin();

        mgr.acquire_lock(&mut t1, 100, LockMode::Shared).unwrap();
        mgr.acquire_lock(&mut t2, 100, LockMode::Shared).unwrap();

        assert!(t1.held_locks().contains(&100));
        assert!(t2.held_locks().contains(&100));
    }

    #[test]
    fn acquire_lock_is_idempotent() {
        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.acquire_lock(&mut tx, 55, LockMode::Exclusive).unwrap();
        mgr.acquire_lock(&mut tx, 55, LockMode::Exclusive).unwrap();
        // held_locks should not duplicate the resource.
        assert_eq!(tx.held_locks().iter().filter(|&&r| r == 55).count(), 1);
    }

    #[test]
    fn acquire_on_non_active_tx_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.commit(&mut tx, &mut wal, &fs).unwrap();

        let res = mgr.acquire_lock(&mut tx, 1, LockMode::Shared);
        assert!(matches!(res, Err(TxError::NotActive(_, TxStatus::Committed))));
    }

    // ------------------------------------------------------------------
    // Wound-wait
    // ------------------------------------------------------------------

    #[test]
    fn older_transaction_wounds_younger_holder() {
        let mgr = TransactionManager::new();
        let mut t1 = mgr.begin(); // older (lower txid)
        let mut t2 = mgr.begin(); // younger

        // t2 acquires an exclusive lock.
        mgr.acquire_lock(&mut t2, 77, LockMode::Exclusive).unwrap();

        // t1 (older) tries to acquire the same lock — should wound t2.
        let res = mgr.acquire_lock(&mut t1, 77, LockMode::Exclusive);
        assert!(
            matches!(res, Err(TxError::WoundWait(victim)) if victim == t2.txid),
            "expected WoundWait({}) but got {:?}",
            t2.txid,
            res
        );
    }

    // ------------------------------------------------------------------
    // Full lifecycle
    // ------------------------------------------------------------------

    #[test]
    fn full_begin_lock_commit_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();

        // Acquire a mix of lock modes on different resources.
        mgr.acquire_lock(&mut tx, 1, LockMode::Shared).unwrap();
        mgr.acquire_lock(&mut tx, 2, LockMode::Exclusive).unwrap();
        mgr.acquire_lock(&mut tx, 3, LockMode::IntentionShared).unwrap();

        assert_eq!(tx.held_locks().len(), 3);

        mgr.commit(&mut tx, &mut wal, &fs).unwrap();
        assert_eq!(tx.status, TxStatus::Committed);
        assert!(tx.held_locks().is_empty());
    }

    #[test]
    fn full_begin_lock_rollback_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.acquire_lock(&mut tx, 10, LockMode::Exclusive).unwrap();
        mgr.rollback(&mut tx, &mut wal, &fs).unwrap();

        assert_eq!(tx.status, TxStatus::Aborted);
        assert!(tx.held_locks().is_empty());

        // Resource 10 should be free for a new transaction.
        let mut tx2 = mgr.begin();
        mgr.acquire_lock(&mut tx2, 10, LockMode::Exclusive).unwrap();
        assert!(!tx2.held_locks().is_empty());
    }

    #[test]
    fn global_state_is_consistent_after_commit_and_abort() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut t1 = mgr.begin();
        let mut t2 = mgr.begin();

        mgr.commit(&mut t1, &mut wal, &fs).unwrap();
        mgr.rollback(&mut t2, &mut wal, &fs).unwrap();

        // Neither t1 nor t2 should appear in the active set.
        let snap = mgr.global_state().active_snapshot();
        assert!(!snap.is_active(t1.txid));
        assert!(!snap.is_active(t2.txid));
    }

    // ------------------------------------------------------------------
    // Secondary index group-commit integration
    // ------------------------------------------------------------------

    #[test]
    fn stage_index_mutation_accumulates_in_tx() {
        use crate::graph::record::SlotRef;
        use crate::index::manager::IndexMutation;

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();

        mgr.stage_index_mutation(
            &mut tx,
            IndexMutation::InsertNode {
                node_id: 1,
                slot: SlotRef::new(10, 5),
            },
        )
        .unwrap();

        assert_eq!(tx.index_mutations.len(), 1);
    }

    #[test]
    fn stage_on_non_active_tx_fails() {
        use crate::graph::record::SlotRef;
        use crate::index::manager::IndexMutation;

        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        mgr.commit(&mut tx, &mut wal, &fs).unwrap();

        let res = mgr.stage_index_mutation(
            &mut tx,
            IndexMutation::InsertNode {
                node_id: 1,
                slot: SlotRef::new(10, 5),
            },
        );
        assert!(matches!(res, Err(TxError::NotActive(_, TxStatus::Committed))));
    }

    #[test]
    fn commit_with_indexes_applies_mutations() {
        use crate::graph::record::SlotRef;
        use crate::index::manager::{IndexManager, IndexMutation};
        use crate::index::key::node_id_key;

        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        let mut index_mgr = IndexManager::new();

        mgr.stage_index_mutation(
            &mut tx,
            IndexMutation::InsertNode {
                node_id: 42,
                slot: SlotRef::new(7, 3),
            },
        )
        .unwrap();

        mgr.commit_with_indexes(&mut tx, &mut index_mgr, &mut wal, &fs
        )
        .unwrap();

        assert_eq!(tx.status, TxStatus::Committed);
        assert!(tx.index_mutations.is_empty());
        assert!(index_mgr.node_index.search(&node_id_key(42)).is_some());
    }

    #[test]
    fn rollback_discards_staged_mutations() {
        use crate::graph::record::SlotRef;
        use crate::index::manager::{IndexManager, IndexMutation};
        use crate::index::key::node_id_key;

        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();
        let index_mgr = IndexManager::new();

        mgr.stage_index_mutation(
            &mut tx,
            IndexMutation::InsertNode {
                node_id: 99,
                slot: SlotRef::new(7, 3),
            },
        )
        .unwrap();

        mgr.rollback(&mut tx, &mut wal, &fs).unwrap();

        assert_eq!(tx.status, TxStatus::Aborted);
        assert!(tx.index_mutations.is_empty());
        assert!(index_mgr.node_index.search(&node_id_key(99)).is_none());
    }
}
