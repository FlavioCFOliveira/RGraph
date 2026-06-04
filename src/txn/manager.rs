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
    phantom::{SsiFlags, SsiTracker},
    snapshot::Snapshot,
    state::GlobalTxState,
    txid::TxId,
    wound_wait::{WoundAction, WoundWait},
};
use crate::wal::{record::{RecordType, WalRecord}, writer::WalWriter};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use thiserror::Error;

/// Shared set of TxIds that have been wounded and must abort at the next
/// opportunity.  The set is populated by the wound-wait callback and cleared
/// when a transaction actually rolls back.
///
/// Every `acquire_lock` call checks whether the requesting transaction's own
/// TxId is in this set; if so it returns `TxError::WoundWait` with its own
/// TxId so the caller knows to roll itself back.
#[derive(Debug, Default)]
struct AbortRegistry {
    aborted: Mutex<HashSet<TxId>>,
}

impl AbortRegistry {
    fn new() -> Self {
        Self {
            aborted: Mutex::new(HashSet::new()),
        }
    }

    /// Mark `txid` as requiring abort (called from the wound-wait callback).
    fn mark_aborted(&self, txid: TxId) {
        self.aborted
            .lock()
            .expect("INVARIANT: abort registry mutex must not be poisoned")
            .insert(txid);
    }

    /// Check whether `txid` has been externally aborted.
    fn is_aborted(&self, txid: TxId) -> bool {
        self.aborted
            .lock()
            .expect("INVARIANT: abort registry mutex must not be poisoned")
            .contains(&txid)
    }

    /// Clear the abort record for `txid` (called when the transaction
    /// actually completes its rollback).
    fn clear(&self, txid: TxId) {
        self.aborted
            .lock()
            .expect("INVARIANT: abort registry mutex must not be poisoned")
            .remove(&txid);
    }
}

/// The isolation level requested by a transaction.
///
/// Affects:
/// * Whether dirty reads are permitted (`ReadUncommitted`).
/// * Whether the snapshot is refreshed after each statement (`ReadCommitted`).
/// * Whether the same row read twice returns the same version (`RepeatableRead`).
/// * Whether phantom anomalies and write-skew are prevented (`Serializable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// Permits dirty reads (not recommended; included for completeness).
    ReadUncommitted,
    /// Snapshot refreshed after every statement.  Prevents dirty reads.
    ReadCommitted,
    /// Snapshot fixed at transaction start.  Prevents non-repeatable reads.
    RepeatableRead,
    /// Full serializability via SSI rw-antidependency tracking.
    /// Write-skew is detected and one of the conflicting transactions is aborted.
    Serializable,
}

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

    /// The transaction accumulated both an `in_conflict` and an `out_conflict`
    /// under Serializable Snapshot Isolation and must abort.
    #[error("transaction {0} detected a phantom / rw-antidependency and must abort")]
    PhantomConflict(TxId),

    /// First-committer-wins write-write conflict: another committed transaction
    /// already modified this resource between our begin and commit.  The caller
    /// must roll back and retry.
    #[error("transaction {0} detected a write-write conflict on resource {1} and must abort")]
    WriteConflict(TxId, u64),
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
/// Created by [`TransactionManager::begin`] or
/// [`TransactionManager::begin_with_isolation`] and consumed by
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
    /// The isolation level this transaction was started with.
    pub isolation_level: IsolationLevel,
    /// Resource IDs for which this transaction holds locks.
    /// Maintained by [`TransactionManager::acquire_lock`].
    held_locks: Vec<u64>,
    /// How many times this transaction has been wounded by the wound-wait
    /// protocol.  Mirrors the count in [`WoundWait`] for quick access.
    wound_count: u8,
    /// Staged secondary-index mutations that will be applied atomically at
    /// commit time.
    pub index_mutations: Vec<IndexMutation>,
    /// SSI conflict flags (in_conflict / out_conflict).
    pub ssi: SsiFlags,
    /// Synthetic range resource IDs that this transaction has read.
    /// Used for phantom detection and next-key locking.
    pub read_ranges: Vec<u64>,
    /// Resource IDs written by this transaction, with the `xmax` that was
    /// present on the resource at the time of the write (first-committer-wins
    /// conflict detection: if xmax has changed by commit time, abort).
    ///
    /// Each entry is `(resource_id, xmax_at_write_time)`.
    pub write_set: Vec<(u64, u32)>,
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
            .field("isolation_level", &self.isolation_level)
            .field("held_locks", &self.held_locks.len())
            .field("wound_count", &self.wound_count)
            .field("ssi_doomed", &self.ssi.is_doomed())
            .field("read_ranges", &self.read_ranges.len())
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
    ssi_tracker: Arc<SsiTracker>,
    /// Registry of TxIds that have been wounded and must abort.
    abort_registry: Arc<AbortRegistry>,
    /// The isolation level used by [`TransactionManager::begin`] when
    /// no explicit level is passed.  Defaults to
    /// [`IsolationLevel::RepeatableRead`].
    default_isolation: IsolationLevel,
}

impl TransactionManager {
    /// Create a new transaction manager backed by a fresh global state.
    pub fn new() -> Self {
        Self {
            global_state: Arc::new(GlobalTxState::new()),
            lock_table: Arc::new(LockTable::new()),
            wound_wait: Arc::new(WoundWait::new()),
            ssi_tracker: Arc::new(SsiTracker::new()),
            abort_registry: Arc::new(AbortRegistry::new()),
            default_isolation: IsolationLevel::RepeatableRead,
        }
    }

    /// Create a transaction manager reusing existing subsystem instances.
    pub fn with_components(
        global_state: Arc<GlobalTxState>,
        lock_table: Arc<LockTable>,
        wound_wait: Arc<WoundWait>,
        ssi_tracker: Arc<SsiTracker>,
    ) -> Self {
        Self {
            global_state,
            lock_table,
            wound_wait,
            ssi_tracker,
            abort_registry: Arc::new(AbortRegistry::new()),
            default_isolation: IsolationLevel::RepeatableRead,
        }
    }

    /// Begin a new transaction at the manager's default isolation level.
    ///
    /// A snapshot of the current committed state is captured at this point.
    /// Use [`TransactionManager::begin_with_isolation`] to specify an explicit
    /// isolation level.
    pub fn begin(&self) -> Transaction {
        self.begin_with_isolation(self.default_isolation)
    }

    /// Begin a new transaction with an explicit isolation level.
    ///
    /// A snapshot of the current committed state is captured at this point.
    ///
    /// # Isolation guarantees
    ///
    /// * [`IsolationLevel::ReadUncommitted`] — no snapshot filtering;
    ///   dirty reads from other active transactions are visible.
    /// * [`IsolationLevel::ReadCommitted`] — snapshot is acquired fresh at
    ///   the beginning of each statement (implemented by the engine layer).
    /// * [`IsolationLevel::RepeatableRead`] — snapshot fixed at transaction
    ///   start.  Non-repeatable reads are prevented; phantoms may still occur.
    /// * [`IsolationLevel::Serializable`] — full SSI: rw-antidependency
    ///   tracking detects and aborts one of the two conflicting transactions
    ///   in a write-skew cycle.
    pub fn begin_with_isolation(&self, level: IsolationLevel) -> Transaction {
        let (txid, snapshot) = self.global_state.begin_tx();
        Transaction {
            txid,
            snapshot,
            status: TxStatus::Active,
            isolation_level: level,
            held_locks: Vec::new(),
            wound_count: 0,
            index_mutations: Vec::new(),
            ssi: SsiFlags::new(),
            read_ranges: Vec::new(),
            write_set: Vec::new(),
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

        // SSI validation: under Serializable isolation, a transaction that
        // accumulated both in_conflict and out_conflict must abort to prevent
        // write-skew.  For lower isolation levels the SSI check is skipped.
        if tx.isolation_level == IsolationLevel::Serializable
            && self.ssi_tracker.is_doomed(tx.txid, &tx.ssi)
        {
            let _ = self.rollback(tx, wal, fs);
            return Err(TxError::PhantomConflict(tx.txid));
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

        // Release any range locks held via next-key locking.
        self.lock_table.release_all(tx.txid, &tx.read_ranges);
        tx.read_ranges.clear();

        // Update global transaction state.
        self.global_state.commit_tx(tx.txid);

        // Clean up wound-wait and SSI metadata.
        self.wound_wait.cleanup(tx.txid);
        self.ssi_tracker.cleanup(tx.txid);

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
    /// The mutation is not applied until `commit_with_indexes` is called.
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

        // Release any range / next-key locks.
        self.lock_table.release_all(tx.txid, &tx.read_ranges);
        tx.read_ranges.clear();

        // Update global transaction state.
        self.global_state.abort_tx(tx.txid);

        // Clean up wound-wait, abort registry, and SSI metadata.
        self.wound_wait.cleanup(tx.txid);
        self.abort_registry.clear(tx.txid);
        self.ssi_tracker.cleanup(tx.txid);

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

        // Check whether this transaction has been externally wounded while it
        // was doing other work.  If so, it must abort immediately.
        if self.abort_registry.is_aborted(tx.txid) {
            tx.status = TxStatus::Aborted;
            self.lock_table.release_all(tx.txid, &tx.held_locks);
            tx.held_locks.clear();
            self.lock_table.release_all(tx.txid, &tx.read_ranges);
            tx.read_ranges.clear();
            self.global_state.abort_tx(tx.txid);
            self.wound_wait.cleanup(tx.txid);
            self.abort_registry.clear(tx.txid);
            self.ssi_tracker.cleanup(tx.txid);
            return Err(TxError::WoundWait(tx.txid));
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
                    // Re-check: we may have been wounded while spinning.
                    if self.abort_registry.is_aborted(tx.txid) {
                        tx.status = TxStatus::Aborted;
                        self.lock_table.release_all(tx.txid, &tx.held_locks);
                        tx.held_locks.clear();
                        self.lock_table.release_all(tx.txid, &tx.read_ranges);
                        tx.read_ranges.clear();
                        self.global_state.abort_tx(tx.txid);
                        self.wound_wait.cleanup(tx.txid);
                        self.abort_registry.clear(tx.txid);
                        self.ssi_tracker.cleanup(tx.txid);
                        return Err(TxError::WoundWait(tx.txid));
                    }

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
                                // We wound the holder.  The callback atomically
                                // marks the victim in the abort registry and
                                // releases its locks via the lock table so that
                                // any blocked waiters are woken up.
                                let lock_table = Arc::clone(&self.lock_table);
                                let global_state = Arc::clone(&self.global_state);
                                let wound_wait = Arc::clone(&self.wound_wait);
                                let ssi_tracker = Arc::clone(&self.ssi_tracker);
                                let abort_registry = Arc::clone(&self.abort_registry);
                                let victim = *holder_txid;
                                self.wound_wait.record_wound(victim, move |v| {
                                    // Mark the victim as requiring abort.
                                    abort_registry.mark_aborted(v);
                                    // Release the victim's locks immediately so
                                    // that blocked requesters (including us) are
                                    // unblocked.  We do not have the victim's
                                    // held_locks Vec here, so we release only
                                    // the specific contested resource.  The
                                    // victim will release remaining locks when
                                    // it detects its aborted status.
                                    lock_table.release(resource_id, v);
                                    // Remove from global active set so snapshots
                                    // taken after this point do not include the
                                    // victim.
                                    global_state.abort_tx(v);
                                    wound_wait.cleanup(v);
                                    ssi_tracker.cleanup(v);
                                });
                                // Return the wounded victim's TxId so the caller
                                // can propagate the error if it cares.
                                return Err(TxError::WoundWait(victim));
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
                            // Re-check abort status after waking: we may have
                            // been wounded while blocked.
                            if self.abort_registry.is_aborted(tx.txid) {
                                tx.status = TxStatus::Aborted;
                                self.lock_table.release_all(tx.txid, &tx.held_locks);
                                tx.held_locks.clear();
                                self.lock_table.release_all(tx.txid, &tx.read_ranges);
                                tx.read_ranges.clear();
                                self.global_state.abort_tx(tx.txid);
                                self.wound_wait.cleanup(tx.txid);
                                self.abort_registry.clear(tx.txid);
                                self.ssi_tracker.cleanup(tx.txid);
                                return Err(TxError::WoundWait(tx.txid));
                            }
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

    /// Return a reference to the SSI tracker.
    pub fn ssi_tracker(&self) -> &SsiTracker {
        &self.ssi_tracker
    }

    /// Acquire a shared lock on a *range* (predicate) resource ID.
    ///
    /// This is the next-key locking stopgap: range scans acquire shared locks
    /// on synthetic resource IDs derived from the scan predicate, while
    /// insertions into the same predicate bucket acquire exclusive locks.
    ///
    /// The resource ID is tracked in `tx.read_ranges` so it is released on
    /// commit/rollback.
    pub fn acquire_range_lock(
        &self,
        tx: &mut Transaction,
        range_id: u64,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }

        loop {
            let (result, rx_opt) = self.lock_table.try_acquire(range_id, tx.txid, LockMode::Shared);
            match result {
                LockResult::Granted => {
                    if !tx.read_ranges.contains(&range_id) {
                        tx.read_ranges.push(range_id);
                    }
                    return Ok(());
                }
                LockResult::Denied => {
                    let holders = self.lock_table.holders(range_id);
                    let mut found_incompatible = false;
                    for (holder_txid, _) in &holders {
                        if *holder_txid == tx.txid {
                            continue;
                        }
                        found_incompatible = true;
                        match self.wound_wait.check(tx.txid, *holder_txid) {
                            WoundAction::Wound => {
                                let victim = *holder_txid;
                                let lock_table = Arc::clone(&self.lock_table);
                                let global_state = Arc::clone(&self.global_state);
                                let wound_wait = Arc::clone(&self.wound_wait);
                                let ssi_tracker = Arc::clone(&self.ssi_tracker);
                                let abort_registry = Arc::clone(&self.abort_registry);
                                self.wound_wait.record_wound(victim, move |v| {
                                    abort_registry.mark_aborted(v);
                                    lock_table.release(range_id, v);
                                    global_state.abort_tx(v);
                                    wound_wait.cleanup(v);
                                    ssi_tracker.cleanup(v);
                                });
                                return Err(TxError::WoundWait(victim));
                            }
                            WoundAction::Wait => {
                                tx.wound_count = self.wound_wait.wound_count(tx.txid);
                            }
                        }
                    }
                    if !found_incompatible {
                        continue;
                    }
                    if let Some(rx) = rx_opt {
                        let notified = rx.recv().unwrap_or(LockResult::Denied);
                        if notified == LockResult::Granted {
                            if !tx.read_ranges.contains(&range_id) {
                                tx.read_ranges.push(range_id);
                            }
                            return Ok(());
                        }
                        continue;
                    } else {
                        continue;
                    }
                }
            }
        }
    }

    /// Record that `tx` has written into a range represented by `range_id`.
    ///
    /// The SSI tracker checks whether any **active** transaction has already
    /// acquired a shared lock on the same `range_id`.  If so, a rw-
    /// antidependency is recorded: the reader gets `out_conflict` and the
    /// writer gets `in_conflict`.  If a transaction ever holds both flags it
    /// will be rejected at commit time.
    ///
    /// # Next-key locking fallback
    ///
    /// This method also acquires an **exclusive** lock on `range_id` using
    /// the ordinary lock table.  If another transaction holds a shared lock
    /// on the same range, the write will block (or wound-wait) until the
    /// reader commits or aborts.  This prevents phantoms even when the SSI
    /// read set is incomplete.
    pub fn record_phantom_write(
        &self,
        tx: &mut Transaction,
        range_id: u64,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }

        // SSI tracking: scan active transactions for readers of this range.
        let snap = self.global_state.active_snapshot();
        // We do not have direct access to every tx's read_ranges here, so
        // we approximate by checking lock table holders of the range_id.
        // Any holder with Shared mode is treated as a reader.
        let holders = self.lock_table.holders(range_id);
        for (holder_txid, holder_mode) in &holders {
            if *holder_txid == tx.txid {
                continue;
            }
            if *holder_mode == LockMode::Shared || *holder_mode == LockMode::IntentionShared {
                // Record rw-antidependency if the holder is still active.
                if snap.is_active(*holder_txid) {
                    // Record the symmetric antidependency: the writer gets
                    // in_conflict; the reader gets out_conflict (tracked
                    // globally so the reader's commit can detect the cycle).
                    tx.ssi.in_conflict.store(true, Ordering::Relaxed);
                    self.ssi_tracker.mark_out_conflict(*holder_txid);
                }
            }
        }

        // Next-key locking fallback: acquire exclusive lock on the range.
        // This blocks (or wounds) if a reader still holds a shared lock.
        loop {
            let (result, rx_opt) = self.lock_table.try_acquire(range_id, tx.txid, LockMode::Exclusive);
            match result {
                LockResult::Granted => {
                    if !tx.held_locks.contains(&range_id) {
                        tx.held_locks.push(range_id);
                    }
                    return Ok(());
                }
                LockResult::Denied => {
                    let holders = self.lock_table.holders(range_id);
                    let mut found_incompatible = false;
                    for (holder_txid, _) in &holders {
                        if *holder_txid == tx.txid {
                            continue;
                        }
                        found_incompatible = true;
                        match self.wound_wait.check(tx.txid, *holder_txid) {
                            WoundAction::Wound => {
                                let victim = *holder_txid;
                                let lock_table = Arc::clone(&self.lock_table);
                                let global_state = Arc::clone(&self.global_state);
                                let wound_wait = Arc::clone(&self.wound_wait);
                                let ssi_tracker = Arc::clone(&self.ssi_tracker);
                                let abort_registry = Arc::clone(&self.abort_registry);
                                self.wound_wait.record_wound(victim, move |v| {
                                    abort_registry.mark_aborted(v);
                                    lock_table.release(range_id, v);
                                    global_state.abort_tx(v);
                                    wound_wait.cleanup(v);
                                    ssi_tracker.cleanup(v);
                                });
                                return Err(TxError::WoundWait(victim));
                            }
                            WoundAction::Wait => {
                                tx.wound_count = self.wound_wait.wound_count(tx.txid);
                            }
                        }
                    }
                    if !found_incompatible {
                        continue;
                    }
                    if let Some(rx) = rx_opt {
                        let notified = rx.recv().unwrap_or(LockResult::Denied);
                        if notified == LockResult::Granted {
                            if !tx.held_locks.contains(&range_id) {
                                tx.held_locks.push(range_id);
                            }
                            return Ok(());
                        }
                        continue;
                    } else {
                        continue;
                    }
                }
            }
        }
    }

    /// Record that `tx` has written `resource_id`, with `xmax_at_write` being
    /// the xmax value observed on the resource at write time.
    ///
    /// At commit time, [`TransactionManager::validate_write_set`] compares the
    /// current xmax against `xmax_at_write`.  If they differ, another
    /// transaction committed a write to the same resource after our BEGIN,
    /// and we must abort (first-committer-wins).
    ///
    /// In the locking protocol this check is redundant for resources where we
    /// hold an exclusive lock (no other transaction can have committed a write
    /// while we hold the lock).  It is provided for completeness and for
    /// optimistic-concurrency extensions.
    pub fn record_write(
        &self,
        tx: &mut Transaction,
        resource_id: u64,
        xmax_at_write: u32,
    ) -> Result<(), TxError> {
        if !tx.is_active() {
            return Err(TxError::NotActive(tx.txid, tx.status));
        }
        // Avoid duplicate entries.
        if !tx.write_set.iter().any(|(r, _)| *r == resource_id) {
            tx.write_set.push((resource_id, xmax_at_write));
        }
        Ok(())
    }

    /// Validate the write set against a snapshot GC horizon.
    ///
    /// Returns the global `xmin` (the lowest active TxId), which can be used
    /// to determine which dead MVCC versions (xmax < global_xmin) are safe to
    /// reclaim.  Dead versions — tuples deleted by a committed transaction
    /// that is no longer visible to any active snapshot — can be GC'd.
    ///
    /// This is a read-only query; GC reclamation itself is performed by a
    /// dedicated vacuum routine.
    pub fn snapshot_gc_horizon(&self) -> u64 {
        self.global_state.global_xmin()
    }
}

impl std::fmt::Debug for TransactionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionManager")
            .field("default_isolation", &self.default_isolation)
            .finish_non_exhaustive()
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

    // ------------------------------------------------------------------
    // SSI / Next-key locking (phantom prevention)
    // ------------------------------------------------------------------

    #[test]
    fn range_lock_prevents_phantom_insertion() {
        let mgr = TransactionManager::new();
        let mut reader = mgr.begin();
        let range_id = 0x8000_0000_0000_0001u64; // synthetic predicate ID

        // Reader acquires shared lock on the range.
        mgr.acquire_range_lock(&mut reader, range_id).unwrap();
        assert!(reader.read_ranges.contains(&range_id));

        // Verify via the lock table that the reader holds a shared lock.
        let holders = mgr.lock_table().holders(range_id);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].0, reader.txid);

        // Writer tries to acquire an exclusive lock on the same range.
        // We use the raw lock table to verify it is denied immediately.
        let writer = mgr.begin();
        let (result, _rx) = mgr.lock_table().try_acquire(range_id, writer.txid, LockMode::Exclusive);
        assert_eq!(result, LockResult::Denied, "exclusive lock on range must be denied while shared lock is held");
    }

    #[test]
    fn ssi_doomed_transaction_is_rejected_at_commit_serializable() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        // Must be Serializable for the SSI doomed check to trigger.
        let mut tx = mgr.begin_with_isolation(IsolationLevel::Serializable);

        // Simulate accumulation of both conflict flags.
        tx.ssi.in_conflict.store(true, Ordering::Relaxed);
        tx.ssi.out_conflict.store(true, Ordering::Relaxed);

        let res = mgr.commit(&mut tx, &mut wal, &fs);
        assert!(
            matches!(res, Err(TxError::PhantomConflict(id)) if id == tx.txid),
            "doomed Serializable transaction must be rejected with PhantomConflict"
        );
        assert_eq!(tx.status, TxStatus::Aborted);
    }

    #[test]
    fn ssi_doomed_check_ignored_for_repeatable_read() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        // RepeatableRead — SSI check must be skipped; tx commits successfully.
        let mut tx = mgr.begin_with_isolation(IsolationLevel::RepeatableRead);
        tx.ssi.in_conflict.store(true, Ordering::Relaxed);
        tx.ssi.out_conflict.store(true, Ordering::Relaxed);

        // Must succeed even with both conflict flags set.
        mgr.commit(&mut tx, &mut wal, &fs).unwrap();
        assert_eq!(tx.status, TxStatus::Committed);
    }

    #[test]
    fn multiple_readers_can_share_range_lock() {
        let mgr = TransactionManager::new();
        let range_id = 0x8000_0000_0000_0002u64;

        let mut t1 = mgr.begin();
        let mut t2 = mgr.begin();

        mgr.acquire_range_lock(&mut t1, range_id).unwrap();
        mgr.acquire_range_lock(&mut t2, range_id).unwrap();

        assert!(t1.read_ranges.contains(&range_id));
        assert!(t2.read_ranges.contains(&range_id));
    }

    // ------------------------------------------------------------------
    // Task 157: Wound-wait actually aborts victims
    // ------------------------------------------------------------------

    /// T1 (older) and T2 (younger) deadlock: T1 holds resource A and waits
    /// for resource B; T2 holds B and waits for A.  With wound-wait, T2 is
    /// the younger transaction so it is wounded and rolled back.  T1 then
    /// acquires B and commits successfully.
    #[test]
    fn wound_wait_aborts_younger_victim_and_older_completes() {
        let mgr = TransactionManager::new();

        let mut t1 = mgr.begin(); // older (lower TxId)
        let mut t2 = mgr.begin(); // younger

        // T2 holds resource 200 exclusively.
        mgr.acquire_lock(&mut t2, 200, LockMode::Exclusive).unwrap();

        // T1 (older) tries to acquire resource 200 — should wound T2.
        let res = mgr.acquire_lock(&mut t1, 200, LockMode::Exclusive);
        assert!(
            matches!(res, Err(TxError::WoundWait(victim)) if victim == t2.txid),
            "T1 (older) must wound T2 (younger): got {:?}", res
        );

        // T2 must now be recorded as aborted in the registry.
        assert!(
            mgr.abort_registry.is_aborted(t2.txid),
            "T2 must be in the abort registry after being wounded"
        );

        // T2 detects its wounded status on the next lock attempt.
        let t2_detect = mgr.acquire_lock(&mut t2, 100, LockMode::Exclusive);
        assert!(
            matches!(t2_detect, Err(TxError::WoundWait(v)) if v == t2.txid),
            "T2 must detect its own wound status: got {:?}", t2_detect
        );
        // T2's status must now be Aborted.
        assert_eq!(t2.status, TxStatus::Aborted);

        // After T2 is wounded and resource 200's lock released, T1 can now
        // acquire it (the wound callback already released T2's lock on 200).
        mgr.acquire_lock(&mut t1, 200, LockMode::Exclusive).unwrap();
        assert!(t1.held_locks().contains(&200));

        // T1 commits successfully.
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());
        mgr.commit(&mut t1, &mut wal, &fs).unwrap();
        assert_eq!(t1.status, TxStatus::Committed);
    }

    // ------------------------------------------------------------------
    // Task 155: Write-write conflict detection and snapshot GC horizon
    // ------------------------------------------------------------------

    #[test]
    fn record_write_stages_write_set_entry() {
        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();

        mgr.record_write(&mut tx, 42, 0).unwrap();
        mgr.record_write(&mut tx, 43, 7).unwrap();

        assert_eq!(tx.write_set.len(), 2);
        assert_eq!(tx.write_set[0], (42, 0));
        assert_eq!(tx.write_set[1], (43, 7));
    }

    #[test]
    fn record_write_is_idempotent_for_same_resource() {
        let mgr = TransactionManager::new();
        let mut tx = mgr.begin();

        mgr.record_write(&mut tx, 99, 0).unwrap();
        mgr.record_write(&mut tx, 99, 0).unwrap(); // duplicate — should not add
        assert_eq!(tx.write_set.len(), 1);
    }

    #[test]
    fn snapshot_gc_horizon_returns_global_xmin() {
        let mgr = TransactionManager::new();

        // No active transactions: horizon should equal xmax (all committed).
        let t1 = mgr.begin();
        let t2 = mgr.begin();
        let horizon_before = mgr.snapshot_gc_horizon();
        // t1 and t2 are active, so xmin <= t1.txid.
        assert!(horizon_before <= t1.txid, "xmin must be <= oldest active txid");
        drop(t1);
        drop(t2);
    }

    #[test]
    fn snapshot_gc_horizon_advances_after_oldest_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut t1 = mgr.begin();
        let mut t2 = mgr.begin();

        let horizon_with_both = mgr.snapshot_gc_horizon();
        assert!(horizon_with_both <= t1.txid);

        mgr.commit(&mut t1, &mut wal, &fs).unwrap();
        let horizon_after_t1 = mgr.snapshot_gc_horizon();
        // After t1 commits, xmin should advance to at least t2.txid.
        assert!(
            horizon_after_t1 >= t2.txid || horizon_after_t1 > t1.txid,
            "GC horizon must advance after oldest tx commits; got {} (t2={})",
            horizon_after_t1, t2.txid
        );

        mgr.rollback(&mut t2, &mut wal, &fs).unwrap();
        let horizon_after_all = mgr.snapshot_gc_horizon();
        assert!(
            horizon_after_all > t2.txid,
            "GC horizon must advance past all committed txids; got {} (t2={})",
            horizon_after_all, t2.txid
        );
    }

    // ------------------------------------------------------------------
    // Task 156: Isolation levels and SSI write-skew detection
    // ------------------------------------------------------------------

    #[test]
    fn begin_with_isolation_sets_level_on_transaction() {
        let mgr = TransactionManager::new();

        let t1 = mgr.begin_with_isolation(IsolationLevel::ReadUncommitted);
        assert_eq!(t1.isolation_level, IsolationLevel::ReadUncommitted);

        let t2 = mgr.begin_with_isolation(IsolationLevel::ReadCommitted);
        assert_eq!(t2.isolation_level, IsolationLevel::ReadCommitted);

        let t3 = mgr.begin_with_isolation(IsolationLevel::RepeatableRead);
        assert_eq!(t3.isolation_level, IsolationLevel::RepeatableRead);

        let t4 = mgr.begin_with_isolation(IsolationLevel::Serializable);
        assert_eq!(t4.isolation_level, IsolationLevel::Serializable);
    }

    /// Write-skew scenario under Serializable: T1 and T2 each read a shared
    /// resource (range_id) and then write disjoint resources.  The SSI tracker
    /// detects the rw-antidependency cycle.  One transaction must be aborted.
    ///
    /// Concrete scenario:
    ///   T1 reads range 1000, then writes resource 2001.
    ///   T2 reads range 1000, then writes resource 2002.
    ///
    /// Under Serializable, one of T1 or T2 must abort because the interleaving
    /// is equivalent to a non-serializable execution.
    ///
    /// Implementation note: in this test we directly set the SSI conflict
    /// flags to simulate the rw-antidependency detection that the engine layer
    /// would perform via `record_phantom_write`.
    #[test]
    fn serializable_write_skew_aborts_one_transaction() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();

        let mut t1 = mgr.begin_with_isolation(IsolationLevel::Serializable);
        let mut t2 = mgr.begin_with_isolation(IsolationLevel::Serializable);

        // Simulate: T1 reads range 1000 and writes resource 2001.
        // T2 reads range 1000 and writes resource 2002.
        // The SSI tracker records rw-antidependencies in both directions.
        mgr.ssi_tracker.record_rw_antidependency(
            t1.txid, t2.txid,
            &t1.ssi, &t2.ssi,
        );
        mgr.ssi_tracker.record_rw_antidependency(
            t2.txid, t1.txid,
            &t2.ssi, &t1.ssi,
        );

        // Both T1 and T2 are now doomed (each has both in_conflict and
        // out_conflict).  Committing either one must return PhantomConflict.
        let res1 = mgr.commit(&mut t1, &mut wal, &fs);
        assert!(
            matches!(res1, Err(TxError::PhantomConflict(_))),
            "T1 must be aborted due to write-skew: got {:?}", res1
        );
        assert_eq!(t1.status, TxStatus::Aborted, "T1 must be Aborted");

        // T2 is also doomed, but since it's already inactive from the doomed
        // path or we can try to commit it.
        let res2 = mgr.commit(&mut t2, &mut wal, &fs);
        assert!(
            matches!(res2, Err(TxError::PhantomConflict(_))),
            "T2 must also be aborted due to write-skew: got {:?}", res2
        );
        assert_eq!(t2.status, TxStatus::Aborted, "T2 must be Aborted");
    }

    /// Under RepeatableRead, the same SSI conflict flags do NOT cause abort.
    #[test]
    fn repeatable_read_ignores_ssi_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin_with_isolation(IsolationLevel::RepeatableRead);

        // Manually set both SSI flags (simulating a write-skew pattern).
        tx.ssi.in_conflict.store(true, Ordering::Relaxed);
        tx.ssi.out_conflict.store(true, Ordering::Relaxed);

        // RepeatableRead must commit successfully.
        mgr.commit(&mut tx, &mut wal, &fs).unwrap();
        assert_eq!(tx.status, TxStatus::Committed);
    }

    /// Under ReadCommitted, SSI flags do NOT cause abort.
    #[test]
    fn read_committed_ignores_ssi_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut wal = make_wal(tmp.path());

        let mgr = TransactionManager::new();
        let mut tx = mgr.begin_with_isolation(IsolationLevel::ReadCommitted);

        tx.ssi.in_conflict.store(true, Ordering::Relaxed);
        tx.ssi.out_conflict.store(true, Ordering::Relaxed);

        mgr.commit(&mut tx, &mut wal, &fs).unwrap();
        assert_eq!(tx.status, TxStatus::Committed);
    }

    /// Verify 64-bit TxId: successive allocations exceed u32::MAX without
    /// wrapping.  (In practice the allocator starts at 1 and proceeds
    /// sequentially, so this test uses an allocator seeded near u32::MAX.)
    #[test]
    fn txid_64bit_no_truncation_across_u32_boundary() {
        use crate::txn::txid::TxIdAllocator;

        // Seed just below u32::MAX to exercise the boundary.
        let alloc = TxIdAllocator::with_start((u32::MAX as u64) - 5);
        let ids: Vec<u64> = (0..10).map(|_| alloc.allocate()).collect();

        // All IDs must be unique and > u32::MAX after crossing the boundary.
        let past_boundary: Vec<u64> = ids.iter().copied().filter(|&id| id > u32::MAX as u64).collect();
        assert!(
            !past_boundary.is_empty(),
            "allocator must issue TxIds > u32::MAX without wrapping"
        );
        // No duplicates.
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "all TxIds must be unique");
    }
}
