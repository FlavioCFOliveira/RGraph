//! Query-dispatch throughput and tail-latency benchmarks (Task 185).
//!
//! "Dispatch" here is the per-request CPU cost the engine pays on the hot path
//! *before* any storage I/O: parsing the Cypher text, running semantic analysis,
//! and producing a logical plan.  In server mode this work happens on every
//! inbound query, so its throughput and tail latency directly bound the
//! engine's predictable-performance mandate.
//!
//! The companion `tests/perf_regression.rs` asserts a hard budget on this same
//! pipeline so CI fails on regression; this bench is the instrument used to set
//! and revisit that budget.  Run it with:
//!
//! ```text
//! cargo bench --bench query_dispatch_bench
//! ```
//!
//! Criterion records per-shape distributions under `target/criterion/`, so
//! tail-latency shifts show up as a widening of the high percentiles across
//! commits (`--save-baseline` / `--baseline` for A/B comparison).

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use rgraph::cypher::parser::parse;
use rgraph::cypher::planner::plan;
use rgraph::cypher::semantic::analyse;

/// Representative query shapes spanning the dispatch cost curve: a trivial
/// RETURN, a single-pattern MATCH with a predicate and projection, a write, and
/// a multi-hop traversal with ordering.
const QUERIES: &[(&str, &str)] = &[
    ("return_literal", "RETURN 1 AS one"),
    (
        "match_where_return",
        "MATCH (n:Person) WHERE n.age > 30 RETURN n.name AS name ORDER BY name",
    ),
    (
        "create_node",
        "CREATE (a:Person {name: 'Alice', age: 30}) RETURN a",
    ),
    (
        "two_hop_expand",
        "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN c.name AS name SKIP 5 LIMIT 10",
    ),
];

/// Benchmark the full front-end dispatch (parse -> semantic -> plan) per shape.
fn dispatch_full(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_dispatch");
    for (name, query) in QUERIES {
        group.bench_with_input(BenchmarkId::from_parameter(name), query, |b, q| {
            b.iter(|| {
                let stmt = parse(black_box(q)).expect("query parses");
                // Semantic analysis can legitimately reject some shapes against
                // an empty schema; the plan step is what we always exercise.
                let _ = analyse(&stmt);
                let logical = plan(&stmt).expect("query plans");
                black_box(logical)
            });
        });
    }
    group.finish();
}

/// Isolate the parser alone — the cheapest, most frequently exercised stage —
/// so a regression in tokenisation/AST construction is attributable.
fn dispatch_parse_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_parse");
    for (name, query) in QUERIES {
        group.bench_with_input(BenchmarkId::from_parameter(name), query, |b, q| {
            b.iter(|| black_box(parse(black_box(q)).expect("query parses")));
        });
    }
    group.finish();
}

criterion_group!(benches, dispatch_full, dispatch_parse_only);
criterion_main!(benches);
