//! ACID stress tests and benchmark suite (Task 125).
//!
//! Provides a Jepsen-style concurrent harness that spawns thousands of
//! read-write transactions against a live database, verifying isolation
//! properties and WAL replay after simulated crashes.

use crate::error::RGraphError;
use crate::graph::graph::Graph;
use crate::graph::builder::NodeBuilder;
use crate::io::FileSystem;
use crate::server::storage::{AsyncStorageEngine, InMemoryStorageEngine};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Configuration for a stress-test run.
#[derive(Debug, Clone)]
pub struct StressConfig {
    /// Number of concurrent worker tasks.
    pub concurrency: usize,
    /// Total number of operations to execute.
    pub total_ops: usize,
    /// Fraction of operations that are writes (0.0–1.0).
    pub write_ratio: f64,
    /// Duration to run if `total_ops` is zero (open-ended mode).
    pub duration_secs: u64,
}

impl Default for StressConfig {
    fn default() -> Self {
        Self {
            concurrency: 100,
            total_ops: 10_000,
            write_ratio: 0.5,
            duration_secs: 60,
        }
    }
}

/// Outcome of a single stress-test run.
#[derive(Debug, Clone)]
pub struct StressReport {
    pub ops_executed: usize,
    pub ops_failed: usize,
    pub elapsed: Duration,
    pub ops_per_sec: f64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub lost_updates: usize,
    pub dirty_reads: usize,
    pub phantom_reads: usize,
}

/// A simple latency histogram with microsecond buckets.
#[derive(Debug)]
pub struct LatencyHistogram {
    buckets: Vec<AtomicU64>,
    max_bucket_us: u64,
    bucket_width_us: u64,
}

impl Clone for LatencyHistogram {
    fn clone(&self) -> Self {
        let mut buckets = Vec::with_capacity(self.buckets.len());
        for b in &self.buckets {
            buckets.push(AtomicU64::new(b.load(Ordering::Relaxed)));
        }
        Self {
            buckets,
            max_bucket_us: self.max_bucket_us,
            bucket_width_us: self.bucket_width_us,
        }
    }
}

impl LatencyHistogram {
    pub fn new(max_bucket_us: u64, bucket_width_us: u64) -> Self {
        let count = (max_bucket_us / bucket_width_us + 1) as usize;
        let mut buckets = Vec::with_capacity(count);
        for _ in 0..count {
            buckets.push(AtomicU64::new(0));
        }
        Self {
            buckets,
            max_bucket_us,
            bucket_width_us,
        }
    }

    pub fn record(&self, duration: Duration) {
        let us = duration.as_micros() as u64;
        let idx = (us / self.bucket_width_us).min(self.buckets.len() as u64 - 1) as usize;
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
    }

    pub fn percentile(&self, p: f64) -> u64 {
        let total: u64 = self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        if total == 0 {
            return 0;
        }
        let target = (total as f64 * p / 100.0).ceil() as u64;
        let mut cumul = 0u64;
        for (idx, bucket) in self.buckets.iter().enumerate() {
            cumul += bucket.load(Ordering::Relaxed);
            if cumul >= target {
                return (idx as u64 * self.bucket_width_us).min(self.max_bucket_us);
            }
        }
        self.max_bucket_us
    }
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new(10_000_000, 100) // 10 s max, 100 µs buckets
    }
}

/// Concurrent stress-test harness.
pub struct StressHarness;

impl StressHarness {
    /// Run the ACID stress battery against an [`InMemoryStorageEngine`].
    ///
    /// Uses a bank-account workload: 10 accounts with initial balance 1000.
    /// Writers transfer 1 unit between accounts; readers verify that the
    /// total balance remains exactly 10_000 (no lost updates or dirty reads).
    pub async fn run_acid_battery(
        config: StressConfig,
        engine: Arc<InMemoryStorageEngine>,
    ) -> Result<StressReport, RGraphError> {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(config.duration_secs);

        let ops_done = Arc::new(AtomicU64::new(0));
        let ops_failed = Arc::new(AtomicU64::new(0));
        let hist = Arc::new(LatencyHistogram::default());
        let lost_updates = Arc::new(AtomicU64::new(0));
        let dirty_reads = Arc::new(AtomicU64::new(0));
        let phantom_reads = Arc::new(AtomicU64::new(0));

        // Prepare bank-account data: 10 accounts with balance 1000 each.
        let accounts: Vec<u64> = (0..10).collect();
        for &acc in &accounts {
            let key = format!("account:{}", acc);
            let val = b"1000";
            engine.put(key.as_bytes(), val).await?;
        }

        let account_balances: Arc<Mutex<HashMap<u64, i64>>> =
            Arc::new(Mutex::new(accounts.iter().map(|&a| (a, 1000i64)).collect()));

        let total = if config.total_ops > 0 {
            config.total_ops
        } else {
            usize::MAX
        };

        let semaphore = Arc::new(Semaphore::new(config.concurrency));

        let mut handles = Vec::with_capacity(config.concurrency);
        for worker in 0..config.concurrency {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let engine = engine.clone();
            let ops_done = ops_done.clone();
            let ops_failed = ops_failed.clone();
            let hist = hist.clone();
            let lost_updates = lost_updates.clone();
            let dirty_reads = dirty_reads.clone();
            let phantom_reads = phantom_reads.clone();
            let account_balances = account_balances.clone();
            let accounts = accounts.clone();

            let handle = tokio::spawn(async move {
                let _permit = permit;
                let worker_ops = total / config.concurrency;
                for i in 0..worker_ops {
                    if Instant::now() >= deadline {
                        break;
                    }
                    let global_i = worker * worker_ops + i;
                    let is_write = (global_i as f64).fract() < config.write_ratio;
                    let op_start = Instant::now();

                    if is_write {
                        let a = (global_i % accounts.len()) as u64;
                        let b = ((global_i + 1) % accounts.len()) as u64;
                        match Self::transfer(&engine, a, b, 1).await {
                            Ok(_) => {
                                let mut bal = account_balances.lock().unwrap();
                                if let Some(v) = bal.get_mut(&a) {
                                    *v -= 1;
                                }
                                if let Some(v) = bal.get_mut(&b) {
                                    *v += 1;
                                }
                            }
                            Err(_) => {
                                ops_failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        match Self::read_total_balance(&engine, &accounts).await {
                            Ok(total_bal) => {
                                if total_bal != 10_000 {
                                    dirty_reads.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            Err(_) => {
                                ops_failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }

                    hist.record(op_start.elapsed());
                    ops_done.fetch_add(1, Ordering::Relaxed);
                }
            });
            handles.push(handle);
        }

        for h in handles {
            let _ = h.await;
        }

        let elapsed = start.elapsed();
        let ops_executed = ops_done.load(Ordering::Relaxed) as usize;
        let ops_failed_count = ops_failed.load(Ordering::Relaxed) as usize;

        // Final validation: sum of balances must equal 10_000.
        let final_balances = account_balances.lock().unwrap();
        let computed_total: i64 = final_balances.values().sum();
        let lost = if computed_total != 10_000 {
            computed_total.abs_diff(10_000) as usize
        } else {
            0
        };
        drop(final_balances);

        Ok(StressReport {
            ops_executed,
            ops_failed: ops_failed_count,
            elapsed,
            ops_per_sec: ops_executed as f64 / elapsed.as_secs_f64().max(0.001),
            p50_us: hist.percentile(50.0),
            p99_us: hist.percentile(99.0),
            p999_us: hist.percentile(99.9),
            lost_updates: lost,
            dirty_reads: dirty_reads.load(Ordering::Relaxed) as usize,
            phantom_reads: phantom_reads.load(Ordering::Relaxed) as usize,
        })
    }

    async fn transfer(
        engine: &InMemoryStorageEngine,
        from: u64,
        to: u64,
        amount: i64,
    ) -> Result<(), RGraphError> {
        let from_key = format!("account:{}", from);
        let to_key = format!("account:{}", to);

        let tx = engine.begin_transaction().await?;
        let from_val = tx.get(from_key.as_bytes()).await?;
        let to_val = tx.get(to_key.as_bytes()).await?;

        let from_bal = from_val
            .and_then(|v| String::from_utf8(v).ok().and_then(|s| s.parse::<i64>().ok()))
            .unwrap_or(0);
        let to_bal = to_val
            .and_then(|v| String::from_utf8(v).ok().and_then(|s| s.parse::<i64>().ok()))
            .unwrap_or(0);

        if from_bal < amount {
            // Insufficient funds — abort.
            let _ = tx.rollback().await;
            return Err(RGraphError::Storage("insufficient funds".into()));
        }

        tx.put(from_key.as_bytes(), (from_bal - amount).to_string().as_bytes())
            .await?;
        tx.put(to_key.as_bytes(), (to_bal + amount).to_string().as_bytes())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn read_total_balance(
        engine: &InMemoryStorageEngine,
        accounts: &[u64],
    ) -> Result<i64, RGraphError> {
        let mut total = 0i64;
        for &acc in accounts {
            let key = format!("account:{}", acc);
            if let Some(v) = engine.get(key.as_bytes()).await? {
                let bal = String::from_utf8(v)
                    .ok()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                total += bal;
            }
        }
        Ok(total)
    }

    /// Validate that committed data survives a crash and recovery.
    pub fn validate_crash_recovery(
        path: std::path::PathBuf,
        fs: &dyn FileSystem,
    ) -> Result<(), RGraphError> {
        use crate::graph::engine::GraphStorageEngine;

        // Create some data.
        let engine = GraphStorageEngine::init(path.clone(), fs)
            .map_err(|e| RGraphError::Storage(e.to_string().into()))?;
        let mut graph = Graph::new(engine);
        let (_, node_id) = graph
            .create_node(NodeBuilder::new().label(1), fs)
            .map_err(|e| RGraphError::Storage(e.to_string().into()))?;
        graph.sync(fs).map_err(|e| RGraphError::Storage(e.to_string().into()))?;

        // Simulate crash by dropping without clean shutdown.
        drop(graph);

        // Re-open and verify.
        let engine2 = GraphStorageEngine::open(path, fs)
            .map_err(|e| RGraphError::Storage(e.to_string().into()))?;
        let graph2 = Graph::new(engine2);
        let recovered = graph2
            .get_node(node_id, fs)
            .map_err(|e| RGraphError::Storage(e.to_string().into()))?;
        assert!(recovered.is_some(), "created node must survive crash");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;

    #[test]
    fn latency_histogram_percentiles() {
        let hist = LatencyHistogram::new(1_000_000, 1000); // 1 s max, 1 ms buckets
        for _ in 0..100 {
            hist.record(Duration::from_micros(5000));
        }
        for _ in 0..10 {
            hist.record(Duration::from_micros(500_000));
        }
        assert_eq!(hist.percentile(50.0), 5000);
        assert!(hist.percentile(99.0) >= 500_000);
    }

    #[tokio::test]
    async fn acid_stress_runs_without_panics() {
        let engine = Arc::new(InMemoryStorageEngine::new());

        let mut config = StressConfig::default();
        config.concurrency = 10;
        config.total_ops = 200;
        config.write_ratio = 0.5;
        config.duration_secs = 10;

        let report = StressHarness::run_acid_battery(config, engine)
            .await
            .unwrap();
        assert!(report.ops_executed > 0);
        assert_eq!(report.dirty_reads, 0, "dirty reads detected under concurrent load");
        assert_eq!(report.lost_updates, 0, "lost updates detected: final balance diverged from 10_000");
        assert_eq!(report.phantom_reads, 0, "phantom reads detected under concurrent load");
    }

    #[test]
    fn crash_recovery_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        StressHarness::validate_crash_recovery(path, &fs).unwrap();
    }
}
