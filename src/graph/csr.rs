//! Compressed Sparse Row (CSR) adjacency layout for read-heavy graph traversals.
//!
//! # Overview
//!
//! The doubly-linked adjacency list in [`crate::graph::adjacency`] is mutable
//! and O(1) for insertions and deletions, but traversal follows random pointers
//! across pages — poor cache behaviour for read-heavy workloads.
//!
//! A [`CsrAdjacency`] snapshot provides a contiguous, **immutable** representation
//! of all edges that is cache-friendly for sequential traversal:
//!
//! ```text
//! row_ptr[src_node_id]     = first index into col_idx for node src_node_id
//! row_ptr[src_node_id + 1] = one-past-last index
//! col_idx[i]               = target node_id of the i-th edge
//! edge_ids[i]              = edge_id   of the i-th edge
//! edge_types[i]            = type_id   of the i-th edge  (SOA column)
//! ```
//!
//! # Node-id semantics
//!
//! The CSR is keyed by the **logical** node ids carried in
//! [`EdgeRecord::source_id`] and [`EdgeRecord::target_id`] (the authoritative
//! endpoint identifiers introduced in Sprint A).  These are *not* the physical
//! slot references; they survive page-layout changes and are not limited to
//! 24-bit page ids.
//!
//! # Freeze / thaw cycle
//!
//! - Call [`CsrBuilder::build`] then [`CsrHolder::freeze`] to atomically publish
//!   a new snapshot.
//! - The snapshot is reference-counted via `Arc`; live readers complete their
//!   traversal even after a newer snapshot is published.
//! - Call [`CsrHolder::thaw`] to discard the current snapshot and fall back to
//!   the mutable linked-list path until the next `freeze`.

use crate::graph::record::{EdgeRecord, NodeRecord, edge_flags};
use std::sync::{Arc, RwLock};

/// An immutable, cache-friendly snapshot of the graph adjacency structure.
///
/// Memory layout uses Struct-of-Arrays (SOA) for edge properties so that
/// traversals over a single property (e.g. type filtering) stride through
/// a single, contiguous array.
#[derive(Debug, Clone)]
pub struct CsrAdjacency {
    /// `row_ptr[node_id]` is the start offset in `col_idx`/`edge_ids` for
    /// the outgoing edges of `node_id`.  Length = `max_node_id + 2`.
    pub row_ptr: Vec<u32>,
    /// Target node-ids of all edges, sorted by source node-id.
    pub col_idx: Vec<u64>,
    /// Edge ids in the same order as `col_idx`.
    pub edge_ids: Vec<u64>,
    /// Edge type ids (SOA column), same order as `col_idx`.
    pub edge_types: Vec<u32>,
    /// The maximum node id represented in this snapshot.
    pub max_node_id: u64,
}

impl CsrAdjacency {
    /// Total number of edges in the snapshot.
    #[inline]
    pub fn edge_count(&self) -> usize {
        self.edge_ids.len()
    }

    /// Total number of nodes (the maximum node id seen).
    #[inline]
    pub fn node_count(&self) -> u64 {
        self.max_node_id
    }

    /// Return the outgoing edges of `src_node_id` as an iterator of
    /// [`CsrEdge`] views.
    ///
    /// Yields nothing if the node has no outgoing edges or if `src_node_id`
    /// exceeds `max_node_id`.
    pub fn outgoing_edges(&self, src_node_id: u64) -> impl Iterator<Item = CsrEdge<'_>> {
        let idx = src_node_id as usize;
        let (start, end) = if idx + 1 < self.row_ptr.len() {
            (self.row_ptr[idx] as usize, self.row_ptr[idx + 1] as usize)
        } else {
            (0, 0)
        };
        (start..end).map(move |i| CsrEdge {
            target_id: self.col_idx[i],
            edge_id: self.edge_ids[i],
            edge_type: self.edge_types[i],
            _phantom: std::marker::PhantomData,
        })
    }

    /// Return the degree (number of outgoing edges) for `src_node_id`.
    pub fn out_degree(&self, src_node_id: u64) -> u32 {
        let idx = src_node_id as usize;
        if idx + 1 < self.row_ptr.len() {
            self.row_ptr[idx + 1] - self.row_ptr[idx]
        } else {
            0
        }
    }
}

/// A single CSR edge entry returned by [`CsrAdjacency::outgoing_edges`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsrEdge<'a> {
    /// Target (logical) node id.
    pub target_id: u64,
    /// Edge id (used for property lookups).
    pub edge_id: u64,
    /// Edge type id.
    pub edge_type: u32,
    // Phantom to tie the lifetime to the `CsrAdjacency` borrow.
    _phantom: std::marker::PhantomData<&'a CsrAdjacency>,
}

/// Builder that constructs a [`CsrAdjacency`] from a flat list of edge records.
pub struct CsrBuilder {
    edges: Vec<(u64, u64, u64, u32)>, // (src_id, dst_id, edge_id, type_id)
    max_node_id: u64,
}

impl CsrBuilder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Self {
            edges: Vec::new(),
            max_node_id: 0,
        }
    }

    /// Add an edge from `rec` to the pending set.
    ///
    /// The CSR is keyed by the logical [`EdgeRecord::source_id`] /
    /// [`EdgeRecord::target_id`].  Deleted (tombstoned) edges are silently
    /// ignored.
    pub fn add_edge(&mut self, rec: &EdgeRecord) {
        if rec.flags & edge_flags::DELETED != 0 {
            return;
        }
        let src = rec.source_id;
        let dst = rec.target_id;
        self.max_node_id = self.max_node_id.max(src).max(dst);
        self.edges.push((src, dst, rec.edge_id, rec.type_id));
    }

    /// Add a node so that `max_node_id` covers it, even if it has no edges.
    pub fn add_node(&mut self, rec: &NodeRecord) {
        self.max_node_id = self.max_node_id.max(rec.node_id);
    }

    /// Consume the builder and produce a [`CsrAdjacency`] snapshot.
    ///
    /// Edges are sorted by `(source_id, target_id, edge_id)` for deterministic,
    /// cache-friendly iteration order.
    pub fn build(mut self) -> CsrAdjacency {
        // Sort by (src, dst, edge_id) for stable, cache-friendly iteration.
        self.edges
            .sort_unstable_by_key(|&(src, dst, eid, _)| (src, dst, eid));

        let n = (self.max_node_id as usize).saturating_add(2); // +2 for sentinel
        let mut row_ptr = vec![0u32; n];

        // Count degree per source node.
        for &(src, _, _, _) in &self.edges {
            if (src as usize) + 1 < n {
                row_ptr[src as usize + 1] += 1;
            }
        }
        // Prefix sum to convert counts into start offsets.
        for i in 1..n {
            row_ptr[i] += row_ptr[i - 1];
        }

        let m = self.edges.len();
        let mut col_idx = vec![0u64; m];
        let mut edge_ids = vec![0u64; m];
        let mut edge_types = vec![0u32; m];

        // Fill the SOA arrays using a per-source cursor.
        let mut cursor = row_ptr.clone();
        for &(src, dst, eid, typ) in &self.edges {
            let pos = cursor[src as usize] as usize;
            col_idx[pos] = dst;
            edge_ids[pos] = eid;
            edge_types[pos] = typ;
            cursor[src as usize] += 1;
        }

        CsrAdjacency {
            row_ptr,
            col_idx,
            edge_ids,
            edge_types,
            max_node_id: self.max_node_id,
        }
    }
}

impl Default for CsrBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Holds the current frozen [`CsrAdjacency`] snapshot.
///
/// Shareable across threads via `Arc<CsrHolder>`.  The inner
/// `RwLock<Option<Arc<CsrAdjacency>>>` makes reads concurrent while `freeze`
/// and `thaw` take a brief exclusive lock only to swap the `Arc`.
pub struct CsrHolder {
    snapshot: RwLock<Option<Arc<CsrAdjacency>>>,
}

impl std::fmt::Debug for CsrHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let has = self.snapshot.read().map(|g| g.is_some()).unwrap_or(false);
        f.debug_struct("CsrHolder")
            .field("has_snapshot", &has)
            .finish()
    }
}

impl CsrHolder {
    /// Create a new empty holder (no snapshot yet).
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(None),
        }
    }

    /// Publish `csr` as the current frozen snapshot.
    ///
    /// The previous snapshot (if any) is dropped, but any `Arc` clones held by
    /// in-flight readers remain valid until those readers drop them.
    pub fn freeze(&self, csr: CsrAdjacency) {
        let arc = Arc::new(csr);
        *self
            .snapshot
            .write()
            .expect("CsrHolder snapshot lock poisoned") = Some(arc);
    }

    /// Release the current frozen snapshot.
    ///
    /// After `thaw`, reads fall back to the mutable linked-list path until the
    /// next `freeze`.
    pub fn thaw(&self) {
        *self
            .snapshot
            .write()
            .expect("CsrHolder snapshot lock poisoned") = None;
    }

    /// Return the current frozen snapshot, or `None` if not yet built.
    ///
    /// The returned `Arc` keeps the snapshot alive for the duration of the
    /// caller's traversal even if a concurrent `freeze` publishes a new one.
    pub fn snapshot(&self) -> Option<Arc<CsrAdjacency>> {
        self.snapshot
            .read()
            .expect("CsrHolder snapshot lock poisoned")
            .clone()
    }

    /// Return `true` if a frozen snapshot is currently available.
    pub fn is_frozen(&self) -> bool {
        self.snapshot
            .read()
            .expect("CsrHolder snapshot lock poisoned")
            .is_some()
    }
}

impl Default for CsrHolder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::record::{EdgeRecord, NodeRecord, SlotRef};

    /// Build an edge whose logical endpoints are `src_id` → `dst_id`.
    fn make_edge(edge_id: u64, type_id: u32, src_id: u64, dst_id: u64) -> EdgeRecord {
        EdgeRecord::new(
            edge_id,
            type_id,
            src_id,
            dst_id,
            SlotRef::new(src_id as u32, 0),
            SlotRef::new(dst_id as u32, 0),
        )
    }

    // -----------------------------------------------------------------
    // CsrBuilder / CsrAdjacency correctness
    // -----------------------------------------------------------------

    #[test]
    fn build_empty_graph() {
        let csr = CsrBuilder::new().build();
        assert_eq!(csr.edge_count(), 0);
        assert_eq!(csr.out_degree(0), 0);
    }

    #[test]
    fn single_edge_round_trip() {
        let mut b = CsrBuilder::new();
        b.add_edge(&make_edge(1, 5, 10, 20));
        let csr = b.build();
        assert_eq!(csr.edge_count(), 1);
        let edges: Vec<_> = csr.outgoing_edges(10).collect();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_id, 20);
        assert_eq!(edges[0].edge_id, 1);
        assert_eq!(edges[0].edge_type, 5);
    }

    #[test]
    fn multiple_outgoing_edges_sorted_by_target() {
        let mut b = CsrBuilder::new();
        b.add_edge(&make_edge(10, 1, 1, 3));
        b.add_edge(&make_edge(20, 1, 1, 2));
        b.add_edge(&make_edge(30, 1, 1, 4));
        let csr = b.build();
        let targets: Vec<u64> = csr.outgoing_edges(1).map(|e| e.target_id).collect();
        // Should be sorted by target id.
        let mut sorted = targets.clone();
        sorted.sort();
        assert_eq!(targets, sorted);
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn nodes_with_no_outgoing_edges_return_empty() {
        let mut b = CsrBuilder::new();
        b.add_node(&NodeRecord::new(99, 0));
        b.add_edge(&make_edge(1, 1, 1, 2));
        let csr = b.build();
        assert_eq!(csr.out_degree(99), 0);
        assert_eq!(csr.outgoing_edges(99).count(), 0);
    }

    #[test]
    fn tombstoned_edges_excluded() {
        let mut e = make_edge(1, 1, 10, 20);
        e.flags = edge_flags::DELETED;
        let mut b = CsrBuilder::new();
        b.add_edge(&e);
        let csr = b.build();
        assert_eq!(csr.edge_count(), 0);
    }

    #[test]
    fn large_graph_row_ptr_prefix_sum_correct() {
        let mut b = CsrBuilder::new();
        // Build a star graph: node 1 → nodes 2..=11.
        for dst in 2u64..=11 {
            b.add_edge(&make_edge(dst, 1, 1, dst));
        }
        let csr = b.build();
        assert_eq!(csr.out_degree(1), 10);
        assert_eq!(csr.out_degree(2), 0);
        let targets: Vec<u64> = csr.outgoing_edges(1).map(|e| e.target_id).collect();
        assert_eq!(targets.len(), 10);
        for (i, &t) in targets.iter().enumerate() {
            assert_eq!(t, (i + 2) as u64, "expected target {} got {}", i + 2, t);
        }
    }

    // -----------------------------------------------------------------
    // CsrHolder freeze / thaw
    // -----------------------------------------------------------------

    #[test]
    fn holder_starts_empty() {
        let h = CsrHolder::new();
        assert!(!h.is_frozen());
        assert!(h.snapshot().is_none());
    }

    #[test]
    fn freeze_makes_snapshot_available() {
        let h = CsrHolder::new();
        let mut b = CsrBuilder::new();
        b.add_edge(&make_edge(1, 1, 10, 20));
        h.freeze(b.build());
        assert!(h.is_frozen());
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.edge_count(), 1);
    }

    #[test]
    fn thaw_removes_snapshot() {
        let h = CsrHolder::new();
        h.freeze(CsrBuilder::new().build());
        assert!(h.is_frozen());
        h.thaw();
        assert!(!h.is_frozen());
    }

    #[test]
    fn old_arc_survives_new_freeze() {
        let h = CsrHolder::new();
        let mut b = CsrBuilder::new();
        b.add_edge(&make_edge(1, 1, 10, 20));
        h.freeze(b.build());
        let old_snap = h.snapshot().unwrap();

        // New freeze with different data.
        let mut b2 = CsrBuilder::new();
        b2.add_edge(&make_edge(2, 1, 10, 30));
        b2.add_edge(&make_edge(3, 1, 10, 40));
        h.freeze(b2.build());

        // Old Arc is still valid and still has 1 edge.
        assert_eq!(old_snap.edge_count(), 1);
        // New snapshot has 2 edges.
        let new_snap = h.snapshot().unwrap();
        assert_eq!(new_snap.edge_count(), 2);
    }

    #[test]
    fn concurrent_read_and_freeze() {
        use std::sync::Arc as StdArc;
        let h = StdArc::new(CsrHolder::new());
        let h2 = h.clone();

        // Pre-load a snapshot.
        let mut b = CsrBuilder::new();
        for i in 1u64..=100 {
            b.add_edge(&make_edge(i, 1, 1, i + 1));
        }
        h.freeze(b.build());

        let reader = std::thread::spawn(move || {
            for _ in 0..1000 {
                if let Some(snap) = h2.snapshot() {
                    let _ = snap.outgoing_edges(1).count();
                }
            }
        });

        // Freeze repeatedly while the reader runs.
        for generation in 0u64..100 {
            let mut b = CsrBuilder::new();
            b.add_edge(&make_edge(generation + 1, 1, 1, 2));
            h.freeze(b.build());
        }

        reader.join().unwrap();
    }
}
