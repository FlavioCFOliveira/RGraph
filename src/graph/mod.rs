//! Graph native storage layer.
//!
//! Provides fixed-size node and edge records, variable-length property
//! storage, doubly-linked adjacency lists, and the [`StorageEngine`]
//! trait implementation.

pub mod record;
pub mod property;
pub mod builder;
// The high-level `Graph` API lives in `graph/graph.rs`; the repeated path
// segment is intentional and renaming the module would churn the public API.
#[allow(clippy::module_inception)]
pub mod graph;
pub mod adjacency;
pub mod csr;
pub mod engine;

pub use record::{EdgeRecord, NodeRecord, PropertyRecord, PropertyHeader, SlotRef, ValueType, OverflowHandle, node_flags, edge_flags};
pub use property::{Property, OrderedF64};
pub use builder::{NodeBuilder, RelationshipBuilder, BuilderError};
pub use graph::{Graph, Node, Relationship};
pub use adjacency::{AdjacencyError, EdgeRecordView, link_source_head, link_target_head, unlink_source, unlink_target, collect_outgoing, collect_incoming};
pub use csr::{CsrAdjacency, CsrBuilder, CsrEdge, CsrHolder};
pub use engine::{GraphStorageEngine, StorageEngine, StorageError};
