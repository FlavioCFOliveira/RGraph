//! Benchmark: CSR adjacency traversal vs mutable linked-list traversal.
//!
//! Compares:
//! - `csr` / `csr_full`: scan outgoing edges via a frozen `CsrAdjacency` snapshot.
//! - `linked_list`:      same via the doubly-linked adjacency walk over an
//!   in-memory `EdgeRecordView`.
//!
//! To run:
//! ```text
//! cargo bench --bench csr_bench
//! ```

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use rgraph::graph::adjacency::{EdgeRecordView, collect_outgoing, link_source_head};
use rgraph::graph::csr::{CsrBuilder, CsrHolder};
use rgraph::graph::record::{EdgeRecord, NodeRecord, SlotRef};
use std::collections::HashMap;

const NODE_COUNTS: &[usize] = &[100, 1_000, 10_000];

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// In-memory edge store backing the linked-list traversal.
struct TestStore {
    edges: HashMap<SlotRef, EdgeRecord>,
}

impl TestStore {
    fn new() -> Self {
        Self {
            edges: HashMap::new(),
        }
    }
}

impl EdgeRecordView for TestStore {
    fn read(&self, slot: SlotRef) -> Option<EdgeRecord> {
        self.edges.get(&slot).copied()
    }
    fn write(&mut self, slot: SlotRef, record: &EdgeRecord) {
        self.edges.insert(slot, *record);
    }
}

/// Populate `node_count` nodes, each with `edges_per_node` outgoing edges.
///
/// Returns `(TestStore, head_per_node, CsrHolder)`.
fn build_graph(node_count: usize, edges_per_node: usize) -> (TestStore, Vec<SlotRef>, CsrHolder) {
    let mut store = TestStore::new();
    let mut heads: Vec<SlotRef> = Vec::with_capacity(node_count);

    // Each edge SlotRef encodes (page_id == edge_id, slot == 0).
    let mut edge_counter: u32 = 1;

    for node in 0..node_count {
        let src_id = node as u64 + 1;
        let src_slot = SlotRef::new(node as u32 + 1, 0);
        let mut head = SlotRef::NULL;
        for j in 0..edges_per_node {
            let dst_id = ((node + j + 1) % node_count) as u64 + 1;
            let dst_slot = SlotRef::new(dst_id as u32, 0);
            let e_slot = SlotRef::new(edge_counter, 0);
            let rec = EdgeRecord::new(
                edge_counter as u64,
                1, // type_id
                src_id,
                dst_id,
                src_slot,
                dst_slot,
            );
            store.edges.insert(e_slot, rec);
            head = link_source_head(&mut store, e_slot, head).unwrap();
            edge_counter += 1;
        }
        heads.push(head);
    }

    // Build the CSR snapshot from the same data.
    let mut builder = CsrBuilder::new();
    for edge in store.edges.values() {
        builder.add_edge(edge);
    }
    for node in 0..node_count {
        builder.add_node(&NodeRecord::new(node as u64 + 1, 0));
    }
    let csr = builder.build();
    let holder = CsrHolder::new();
    holder.freeze(csr);

    (store, heads, holder)
}

// ------------------------------------------------------------------
// Benchmarks
// ------------------------------------------------------------------

fn bench_linked_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("adjacency_traversal/linked_list");
    for &n in NODE_COUNTS {
        let (store, heads, _) = build_graph(n, 5);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut total = 0usize;
                for head in &heads {
                    let edges = collect_outgoing(&store, *head);
                    total += black_box(edges.len());
                }
                total
            });
        });
    }
    group.finish();
}

fn bench_csr(c: &mut Criterion) {
    let mut group = c.benchmark_group("adjacency_traversal/csr");
    for &n in NODE_COUNTS {
        let (_, _, holder) = build_graph(n, 5);
        let snap = holder.snapshot().unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let mut total = 0usize;
                for node_id in 1u64..=(n as u64) {
                    let degree = snap.out_degree(node_id);
                    total += black_box(degree as usize);
                }
                total
            });
        });
    }
    group.finish();
}

fn bench_csr_full_traversal(c: &mut Criterion) {
    let mut group = c.benchmark_group("adjacency_traversal/csr_full");
    for &n in NODE_COUNTS {
        let (_, _, holder) = build_graph(n, 5);
        let snap = holder.snapshot().unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let mut total = 0usize;
                for node_id in 1u64..=(n as u64) {
                    for e in snap.outgoing_edges(node_id) {
                        total += black_box(e.target_id as usize);
                    }
                }
                total
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_linked_list, bench_csr, bench_csr_full_traversal);
criterion_main!(benches);
