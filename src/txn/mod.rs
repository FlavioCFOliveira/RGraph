//! Transaction subsystem for RGraph.
//!
//! This module provides a complete ACID transaction layer with snapshot-isolation
//! MVCC semantics, sharded lock management, and wound-wait deadlock prevention.
//!
//! # Module structure
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`txid`] | Monotonic TxID allocator (atomic, crash-safe) |
//! | [`mvcc`] | MVCC tuple version headers for slotted pages |
//! | [`snapshot`] | Snapshot acquisition and tuple-visibility evaluator |
//! | [`state`] | Global transaction state (xmin/xmax/active set) |
//! | [`lock_table`] | Sharded lock table with FIFO wait queues |
//! | [`wound_wait`] | Wound-wait deadlock prevention oracle |
//! | [`manager`] | Transaction manager — the primary public entry point |
//!
//! # Typical usage
//!
//! ```rust,no_run
//! use rgraph::txn::manager::{TransactionManager, TxStatus};
//! use rgraph::txn::lock_table::LockMode;
//!
//! // Create the manager (typically shared behind an Arc in production).
//! let mgr = TransactionManager::new();
//!
//! // Begin a transaction.
//! let mut tx = mgr.begin();
//! assert_eq!(tx.status, TxStatus::Active);
//!
//! // Acquire a lock (shared or exclusive) on a resource.
//! // mgr.acquire_lock(&mut tx, resource_id, LockMode::Exclusive).unwrap();
//!
//! // Commit or roll back.
//! // mgr.commit(&mut tx, &mut wal, &fs).unwrap();
//! // mgr.rollback(&mut tx, &mut wal, &fs).unwrap();
//! ```

pub mod txid;
pub mod mvcc;
pub mod snapshot;
pub mod state;
pub mod lock_table;
pub mod wound_wait;
pub mod manager;
