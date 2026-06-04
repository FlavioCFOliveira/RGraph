//! Performance-regression guard for the query-dispatch hot path (Task 185).
//!
//! The `query_dispatch_bench` criterion benchmark is the *instrument* used to
//! characterise dispatch latency; this libtest suite is the *gate* that fails CI
//! when that latency regresses by an order of magnitude — an accidental O(n²),
//! a per-query heap-allocation explosion, or a lock added to the front end.
//!
//! # Why a coarse budget rather than a tight one
//!
//! Wall-clock budgets in unit tests must survive heterogeneous CI hardware
//! (shared runners, ARM vs x86, thermal throttling).  A tight bound would be
//! flaky; a 10–50x-headroom bound still catches the regressions that matter
//! (algorithmic blow-ups, not 20% drift — drift is criterion's job).  The
//! reference point was measured on a Raspberry Pi 5 (Cortex-A76 @ 2.4 GHz, the
//! slow end of the deployment range): full parse+semantic+plan dispatch of the
//! heaviest representative query runs in single-digit microseconds.
//!
//! # Tuning on slow or contended CI
//!
//! Every budget is overridable via an environment variable (a positive
//! multiplier applied to the default).  Set `RGRAPH_PERF_SLACK=4` to quadruple
//! every budget on a known-slow runner without editing the test.

use rgraph::cypher::parser::parse;
use rgraph::cypher::planner::plan;
use rgraph::cypher::semantic::analyse;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Heaviest representative dispatch query (multi-hop pattern + ordering +
/// pagination): the worst case among the shapes the bench tracks.
const HEAVY_QUERY: &str =
    "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN c.name AS name SKIP 5 LIMIT 10";

/// Cheapest dispatch query: a literal RETURN.  A regression here points at the
/// parser/tokeniser rather than planning.
const LIGHT_QUERY: &str = "RETURN 1 AS one";

/// Number of measured iterations.  Large enough that the median is stable and
/// per-iteration timer overhead is amortised, small enough to stay well under a
/// second even on the slowest target.
const ITERATIONS: usize = 20_000;

/// Read the user-supplied slack multiplier (default 1.0).  Values ≤ 0 or
/// unparseable fall back to 1.0 so a malformed override never disables the gate.
fn perf_slack() -> f64 {
    std::env::var("RGRAPH_PERF_SLACK")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&m| m > 0.0)
        .unwrap_or(1.0)
}

/// Run one full dispatch (parse -> semantic -> plan) of `query`, returning a
/// value the optimiser cannot discard.
#[inline]
fn dispatch(query: &str) -> usize {
    let stmt = parse(black_box(query)).expect("query must parse");
    let _ = analyse(&stmt);
    let logical = plan(&stmt).expect("query must plan");
    // Touch the result so the whole pipeline is observably live.
    black_box(&logical) as *const _ as usize
}

/// Measure the median and total wall time of `ITERATIONS` dispatches of `query`
/// after a warm-up pass.  Returns `(median_per_op, total)`.
fn measure(query: &str) -> (Duration, Duration) {
    // Warm up: prime caches, branch predictors, and any one-shot allocations.
    for _ in 0..(ITERATIONS / 10).max(1) {
        black_box(dispatch(query));
    }

    let mut samples: Vec<Duration> = Vec::with_capacity(ITERATIONS);
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let t0 = Instant::now();
        black_box(dispatch(query));
        samples.push(t0.elapsed());
    }
    let total = start.elapsed();

    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    (median, total)
}

#[test]
fn heavy_query_dispatch_stays_within_budget() {
    // 500 µs median budget: ~70x headroom over the ~7 µs Pi-5 reference, so only
    // an order-of-magnitude regression trips it.
    let budget = Duration::from_micros(500).mul_f64(perf_slack());

    let (median, total) = measure(HEAVY_QUERY);
    eprintln!(
        "heavy dispatch: median={:?} total={:?} over {} iters (budget {:?})",
        median, total, ITERATIONS, budget
    );

    assert!(
        median <= budget,
        "heavy-query dispatch median {:?} exceeded budget {:?} \
         (set RGRAPH_PERF_SLACK to relax on slow CI)",
        median,
        budget
    );
}

#[test]
fn light_query_dispatch_stays_within_budget() {
    // 100 µs median budget for a literal RETURN (~1 µs Pi-5 reference).
    let budget = Duration::from_micros(100).mul_f64(perf_slack());

    let (median, total) = measure(LIGHT_QUERY);
    eprintln!(
        "light dispatch: median={:?} total={:?} over {} iters (budget {:?})",
        median, total, ITERATIONS, budget
    );

    assert!(
        median <= budget,
        "light-query dispatch median {:?} exceeded budget {:?} \
         (set RGRAPH_PERF_SLACK to relax on slow CI)",
        median,
        budget
    );
}

#[test]
fn dispatch_throughput_floor() {
    // Throughput floor: the engine must sustain at least this many heavy-query
    // dispatches per second on the front end.  2 000 ops/s is a deliberately low
    // floor (the Pi-5 reference is ~140 000 ops/s) so it only fails on a true
    // collapse, not on hardware variance.
    let floor_ops_per_sec = 2_000.0 / perf_slack();

    let (_median, total) = measure(HEAVY_QUERY);
    let ops_per_sec = ITERATIONS as f64 / total.as_secs_f64();
    eprintln!(
        "heavy dispatch throughput: {:.0} ops/s (floor {:.0} ops/s)",
        ops_per_sec, floor_ops_per_sec
    );

    assert!(
        ops_per_sec >= floor_ops_per_sec,
        "heavy-query dispatch throughput {:.0} ops/s fell below floor {:.0} ops/s \
         (set RGRAPH_PERF_SLACK to relax on slow CI)",
        ops_per_sec,
        floor_ops_per_sec
    );
}
