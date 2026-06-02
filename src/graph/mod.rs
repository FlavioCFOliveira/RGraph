//! Graph native storage layer.
//!
//! Provides fixed-size node and edge records, variable-length property
//! storage, doubly-linked adjacency lists, and the [`StorageEngine`]
//! trait implementation.

pub mod record;
pub mod adjacency;
pub mod engine;

pub use record::{EdgeRecord, NodeRecord, PropertyRecord, PropertyHeader, SlotRef, ValueType, OverflowHandle, node_flags, edge_flags};
pub use adjacency::{AdjacencyError, EdgeRecordView, link_source_head, link_target_head, unlink_source, unlink_target, collect_outgoing, collect_incoming};
pub use engine::{GraphStorageEngine, StorageEngine, StorageError};
