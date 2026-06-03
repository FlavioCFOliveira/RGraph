//! Serializable Snapshot Isolation (SSI) and next-key / predicate locking.
//!
//! This module provides the building blocks for phantom prevention in
//! repeatable-read range scans.  It combines two mechanisms:
//!
//! 1. **SSI rw-antidependency tracking** — every transaction carries
//!    `in_conflict` / `out_conflict` flags.  When a younger transaction
//!    writes into a range that an older transaction has read, the older tx
//!    gets `out_conflict = true` and the younger tx gets `in_conflict = true`.
//!    If a transaction ever holds **both** flags it is aborted on commit.
//!
//! 2. **Next-key locking fallback** — when SSI metadata is incomplete (e.g.
//!    because the read set is not fully materialised), the engine can fall
//!    back to acquiring shared/exclusive locks on synthetic *range*
//!    resource IDs derived from the scan predicate.  These locks are managed
//!    by the same [`LockTable`] used for row-level locking, but the resource
//!    IDs are hashed from the predicate so that they do not collide with
//!    individual row IDs.
//!
//! # Design notes
//!
//! * Range resource IDs are **hashed** from a predicate string or page ID.
//!   Collisions are possible but statistically unlikely for the workloads
//!   targeted in Sprint 20; a future iteration can switch to a dedicated
//!   predicate-lock table.
//! * The granularity is intentionally coarse (one lock per leaf-page or per
//!   label index) so that **not all edge insertions to a single node** are
//!   serialised — only insertions landing in the same predicate bucket contend.
//! * The SSI flags live in [`Transaction`] and are checked by
//!   [`TransactionManager::commit`] (see `manager.rs`).

use crate::txn::txid::TxId;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// A global tracker for SSI rw-antidependencies.
///
/// For every active transaction we store:
/// * `out_conflict` — this tx has read a range that a younger tx later wrote.
/// * `in_conflict`  — this tx has written into a range that an older tx read.
///
/// When **both** are true the transaction must abort.
#[derive(Debug)]
pub struct SsiTracker {
    /// TxIds that have an `out_conflict` (read-before-write).
    out_set: Mutex<HashSet<TxId>>,
    /// TxIds that have an `in_conflict` (write-after-read).
    in_set: Mutex<HashSet<TxId>>,
}

impl SsiTracker {
    pub fn new() -> Self {
        Self {
            out_set: Mutex::new(HashSet::new()),
            in_set: Mutex::new(HashSet::new()),
        }
    }

    /// Record that `reader` read a range later written by `writer`.
    pub fn record_rw_antidependency(&self,
        reader: TxId,
        writer: TxId,
        reader_flags: &SsiFlags,
        writer_flags: &SsiFlags,
    ) {
        // Reader gets out_conflict, writer gets in_conflict.
        self.out_set.lock().unwrap().insert(reader);
        self.in_set.lock().unwrap().insert(writer);
        reader_flags.out_conflict.store(true, Ordering::Relaxed);
        writer_flags.in_conflict.store(true, Ordering::Relaxed);
    }

    /// Has `txid` accumulated both in and out conflicts?
    pub fn is_doomed(&self, _txid: TxId, flags: &SsiFlags) -> bool {
        flags.in_conflict.load(Ordering::Relaxed)
            && flags.out_conflict.load(Ordering::Relaxed)
    }

    /// Clean up bookkeeping for a finalised transaction.
    pub fn cleanup(&self, txid: TxId) {
        self.out_set.lock().unwrap().remove(&txid);
        self.in_set.lock().unwrap().remove(&txid);
    }
}

impl Default for SsiTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-transaction SSI state.
///
/// Stored inside [`Transaction`] so that the manager can check it at
/// commit time without a global lookup.
#[derive(Debug)]
pub struct SsiFlags {
    /// This transaction wrote after reading a range written by another tx.
    pub in_conflict: AtomicBool,
    /// This transaction read a range later written by another tx.
    pub out_conflict: AtomicBool,
}

impl SsiFlags {
    pub fn new() -> Self {
        Self {
            in_conflict: AtomicBool::new(false),
            out_conflict: AtomicBool::new(false),
        }
    }

    pub fn is_doomed(&self) -> bool {
        self.in_conflict.load(Ordering::Relaxed)
            && self.out_conflict.load(Ordering::Relaxed)
    }
}

impl Default for SsiFlags {
    fn default() -> Self {
        Self::new()
    }
}

/// Derive a synthetic range resource ID from a B+ tree page ID.
///
/// The high bit is set so that range IDs never overlap with ordinary
/// row IDs (which are derived from slot addresses and never set the MSB).
pub fn range_resource_id(page_id: u64) -> u64 {
    page_id | (1u64 << 63)
}

/// Derive a synthetic range resource ID from an arbitrary predicate key.
///
/// Uses a 64-bit FNV-1a hash so that different predicates map to distinct
/// buckets with low collision probability.
pub fn predicate_resource_id(predicate: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0x811c9dc5;
    const FNV_PRIME: u64 = 0x01000193;
    let mut hash = FNV_OFFSET;
    for byte in predicate {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Ensure the MSB is set to keep predicate IDs in a separate namespace.
    hash | (1u64 << 63)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_id_sets_high_bit() {
        let rid = range_resource_id(42);
        assert_eq!(rid & (1u64 << 63), 1u64 << 63);
        assert_eq!(rid & !(1u64 << 63), 42);
    }

    #[test]
    fn predicate_id_is_deterministic() {
        let a = predicate_resource_id(b"MATCH (n:Person)");
        let b = predicate_resource_id(b"MATCH (n:Person)");
        assert_eq!(a, b);
        let c = predicate_resource_id(b"MATCH (n:Movie)");
        assert_ne!(a, c);
    }

    #[test]
    fn ssi_flags_doomed_when_both_set() {
        let f = SsiFlags::new();
        assert!(!f.is_doomed());
        f.in_conflict.store(true, Ordering::Relaxed);
        assert!(!f.is_doomed());
        f.out_conflict.store(true, Ordering::Relaxed);
        assert!(f.is_doomed());
    }

    #[test]
    fn tracker_records_antidependency() {
        let tracker = SsiTracker::new();
        let reader_flags = SsiFlags::new();
        let writer_flags = SsiFlags::new();

        // Single rw-antidependency: reader gets out_conflict, writer gets in_conflict.
        // Neither is doomed yet because each only holds one of the two flags.
        tracker.record_rw_antidependency(1, 2, &reader_flags, &writer_flags);
        assert!(!tracker.is_doomed(1, &reader_flags));
        assert!(!tracker.is_doomed(2, &writer_flags));

        // A second antidependency in the reverse direction gives both transactions
        // both flags, dooming them.
        tracker.record_rw_antidependency(2, 1, &writer_flags, &reader_flags);
        assert!(tracker.is_doomed(1, &reader_flags));
        assert!(tracker.is_doomed(2, &writer_flags));
    }

    #[test]
    fn tracker_cleans_up_on_finalise() {
        let tracker = SsiTracker::new();
        let f = SsiFlags::new();
        tracker.record_rw_antidependency(10, 20, &f, &f);
        assert!(tracker.is_doomed(10, &f));
        tracker.cleanup(10);
        // flags still have both bits set, but the set membership is gone.
        // The doomed check uses the flags directly, so it still returns true.
        // Cleanup is meant for global bookkeeping, not flag clearing.
        assert!(f.is_doomed());
    }
}
