//! A real benchmark loop driving the graph engine.
//!
//! [`run`] performs `inserts` node creations followed by `reads` point lookups
//! against the live [`Graph`], timing every operation with a monotonic clock
//! ([`std::time::Instant`]).  It reports throughput (ops/sec) and tail latency
//! percentiles (p50/p95/p99) per phase.

use crate::graph::builder::NodeBuilder;
use crate::graph::graph::Graph;
use crate::io::FileSystem;
use std::time::{Duration, Instant};

/// Latency statistics for one benchmark phase, derived from per-op samples.
#[derive(Debug, Clone, Copy)]
pub struct LatencyStats {
    /// Number of operations measured.
    pub count: usize,
    /// Wall-clock duration of the whole phase.
    pub elapsed: Duration,
    /// Throughput in operations per second over the phase.
    pub ops_per_sec: f64,
    /// Median per-op latency.
    pub p50: Duration,
    /// 95th-percentile per-op latency.
    pub p95: Duration,
    /// 99th-percentile per-op latency.
    pub p99: Duration,
    /// Maximum per-op latency observed.
    pub max: Duration,
}

impl LatencyStats {
    /// Compute statistics from a slice of per-operation latencies and the total
    /// phase `elapsed` time.
    fn from_samples(mut samples: Vec<Duration>, elapsed: Duration) -> Self {
        let count = samples.len();
        if count == 0 {
            return Self {
                count: 0,
                elapsed,
                ops_per_sec: 0.0,
                p50: Duration::ZERO,
                p95: Duration::ZERO,
                p99: Duration::ZERO,
                max: Duration::ZERO,
            };
        }
        samples.sort_unstable();
        let pct = |p: f64| -> Duration {
            // Nearest-rank percentile; clamp the index into bounds.
            let rank = ((p / 100.0) * count as f64).ceil() as usize;
            let idx = rank.saturating_sub(1).min(count - 1);
            samples[idx]
        };
        let ops_per_sec = if elapsed.as_secs_f64() > 0.0 {
            count as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        Self {
            count,
            elapsed,
            ops_per_sec,
            p50: pct(50.0),
            p95: pct(95.0),
            p99: pct(99.0),
            max: samples[count - 1],
        }
    }
}

/// Aggregate result of a benchmark run.
#[derive(Debug, Clone, Copy)]
pub struct BenchmarkReport {
    /// Statistics for the insert phase.
    pub insert: LatencyStats,
    /// Statistics for the point-read phase.
    pub read: LatencyStats,
}

impl BenchmarkReport {
    /// Render a fixed-width summary table suitable for stdout.
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str("phase   |      ops |   ops/sec |      p50 |      p95 |      p99 |      max\n");
        out.push_str("--------+----------+-----------+----------+----------+----------+---------\n");
        out.push_str(&format_row("insert", &self.insert));
        out.push_str(&format_row("read", &self.read));
        out
    }
}

fn format_row(name: &str, s: &LatencyStats) -> String {
    format!(
        "{:<7} | {:>8} | {:>9.0} | {:>8} | {:>8} | {:>8} | {:>8}\n",
        name,
        s.count,
        s.ops_per_sec,
        fmt_dur(s.p50),
        fmt_dur(s.p95),
        fmt_dur(s.p99),
        fmt_dur(s.max),
    )
}

/// Format a duration compactly in the most readable unit.
fn fmt_dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}us", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

/// Run an insert-then-read benchmark against `graph`.
///
/// * `inserts` nodes are created (each a small labelled node with one
///   property), timing each `create_node` call.
/// * `reads` point lookups are then performed against the just-created ids,
///   cycling if `reads > inserts`.
///
/// The engine is synced after the insert phase so reads exercise the durable
/// path.  Returns a [`BenchmarkReport`]; the caller decides how to present it.
///
/// # Errors
///
/// Propagates the first storage error encountered.
pub fn run(
    graph: &mut Graph,
    inserts: usize,
    reads: usize,
    fs: &dyn FileSystem,
) -> Result<BenchmarkReport, crate::graph::engine::StorageError> {
    // --- Insert phase ---
    let mut insert_samples = Vec::with_capacity(inserts);
    let mut ids = Vec::with_capacity(inserts);
    let insert_start = Instant::now();
    for i in 0..inserts {
        let builder = NodeBuilder::new()
            .label(1)
            .property("seq", i as i64);
        let op_start = Instant::now();
        let (_slot, id) = graph.create_node(builder, fs)?;
        insert_samples.push(op_start.elapsed());
        ids.push(id);
    }
    let insert_elapsed = insert_start.elapsed();

    // Make the inserted data durable before the read phase.
    graph.sync(fs)?;

    // --- Read phase ---
    let mut read_samples = Vec::with_capacity(reads);
    let read_start = Instant::now();
    for i in 0..reads {
        let id = if ids.is_empty() {
            // Nothing was inserted; probe a non-existent id so the phase still
            // measures the lookup path.
            (i as u64) + 1
        } else {
            ids[i % ids.len()]
        };
        let op_start = Instant::now();
        let _ = graph.get_node(id, fs)?;
        read_samples.push(op_start.elapsed());
    }
    let read_elapsed = read_start.elapsed();

    Ok(BenchmarkReport {
        insert: LatencyStats::from_samples(insert_samples, insert_elapsed),
        read: LatencyStats::from_samples(read_samples, read_elapsed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::engine::GraphStorageEngine;
    use crate::io::posix::PosixFileSystem;

    fn temp_graph() -> (tempfile::TempDir, PosixFileSystem, Graph) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        let engine = GraphStorageEngine::init(path, &fs).unwrap();
        (dir, fs, Graph::new(engine))
    }

    #[test]
    fn percentiles_are_monotonic() {
        let samples: Vec<Duration> = (1..=100).map(|n| Duration::from_micros(n)).collect();
        let stats = LatencyStats::from_samples(samples, Duration::from_millis(10));
        assert!(stats.p50 <= stats.p95);
        assert!(stats.p95 <= stats.p99);
        assert!(stats.p99 <= stats.max);
        assert_eq!(stats.count, 100);
        assert!(stats.ops_per_sec > 0.0);
    }

    #[test]
    fn empty_samples_are_zeroed() {
        let stats = LatencyStats::from_samples(Vec::new(), Duration::from_millis(1));
        assert_eq!(stats.count, 0);
        assert_eq!(stats.ops_per_sec, 0.0);
        assert_eq!(stats.p99, Duration::ZERO);
    }

    #[test]
    fn benchmark_runs_and_reports_nonzero_throughput() {
        let (_dir, fs, mut graph) = temp_graph();
        let report = run(&mut graph, 50, 50, &fs).unwrap();
        assert_eq!(report.insert.count, 50);
        assert_eq!(report.read.count, 50);
        assert!(
            report.insert.ops_per_sec > 0.0,
            "insert throughput must be positive"
        );
        assert!(
            report.read.ops_per_sec > 0.0,
            "read throughput must be positive"
        );
        // Table renders without panicking and includes both phases.
        let table = report.render_table();
        assert!(table.contains("insert"));
        assert!(table.contains("read"));
    }

    #[test]
    fn benchmark_with_zero_inserts_still_measures_reads() {
        let (_dir, fs, mut graph) = temp_graph();
        let report = run(&mut graph, 0, 10, &fs).unwrap();
        assert_eq!(report.insert.count, 0);
        assert_eq!(report.read.count, 10);
    }
}
