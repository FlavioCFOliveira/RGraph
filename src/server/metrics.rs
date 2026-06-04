//! Metrics exporter and health-check endpoint (Task 54).
//!
//! Uses the [`prometheus`] crate to maintain counters, histograms, and gauges.
//! The [`HealthService`] gRPC handler and an optional HTTP `/health` endpoint
//! both read from the same [`MetricsCollector`] instance.

use prometheus::{
    histogram_opts, opts, Counter, Gauge, Histogram, Registry, TextEncoder,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Central collector for all server metrics.
///
/// Every metric handle is registered with a **dedicated, per-server**
/// [`Registry`] (see [`MetricsCollector::registry`]) rather than the
/// process-global default registry.  This keeps multiple in-process server
/// instances (and tests) isolated from one another and avoids the
/// "already registered" failures that the shared default registry produces.
pub struct MetricsCollector {
    /// The per-server Prometheus registry that owns all the metrics below.
    registry: Registry,
    /// Total number of queries executed.
    pub queries_total: Counter,
    /// Total number of failed queries.
    pub queries_failed: Counter,
    /// Query latency histogram (seconds).
    pub query_latency: Histogram,
    /// Number of active connections.
    pub active_connections: Gauge,
    /// Total number of connections opened over the server's lifetime.
    pub connections_opened: Counter,
    /// Total number of connections closed over the server's lifetime.
    pub connections_closed: Counter,
    /// Total number of transactions begun.
    pub transactions_total: Counter,
    /// Total number of transactions committed.
    pub transactions_committed: Counter,
    /// Total number of transactions aborted (rollback or conflict).
    pub transactions_aborted: Counter,
    /// Total buffer-pool cache hits.
    pub cache_hits: Counter,
    /// Total buffer-pool cache misses.
    pub cache_misses: Counter,
    /// Buffer-pool hit rate (0.0–1.0).
    pub cache_hit_rate: Gauge,
}

impl MetricsCollector {
    /// Create a new collector backed by a fresh per-server [`Registry`].
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        let queries_total = Counter::with_opts(opts!(
            "rgraph_queries_total",
            "Total number of Cypher queries received"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(queries_total.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let queries_failed = Counter::with_opts(opts!(
            "rgraph_queries_failed_total",
            "Total number of Cypher queries that returned an error"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(queries_failed.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let query_latency = Histogram::with_opts(histogram_opts!(
            "rgraph_query_latency_seconds",
            "Query execution latency",
            vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(query_latency.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let active_connections = Gauge::with_opts(opts!(
            "rgraph_active_connections",
            "Number of currently open client connections"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(active_connections.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let connections_opened = Counter::with_opts(opts!(
            "rgraph_connections_opened_total",
            "Total number of client connections accepted"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(connections_opened.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let connections_closed = Counter::with_opts(opts!(
            "rgraph_connections_closed_total",
            "Total number of client connections closed"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(connections_closed.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let transactions_total = Counter::with_opts(opts!(
            "rgraph_transactions_total",
            "Total number of transactions begun"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(transactions_total.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let transactions_committed = Counter::with_opts(opts!(
            "rgraph_transactions_committed_total",
            "Total number of transactions committed"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(transactions_committed.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let transactions_aborted = Counter::with_opts(opts!(
            "rgraph_transactions_aborted_total",
            "Total number of transactions aborted (rollback or conflict)"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(transactions_aborted.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let cache_hits = Counter::with_opts(opts!(
            "rgraph_cache_hits_total",
            "Total number of buffer-pool cache hits"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(cache_hits.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let cache_misses = Counter::with_opts(opts!(
            "rgraph_cache_misses_total",
            "Total number of buffer-pool cache misses"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(cache_misses.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        let cache_hit_rate = Gauge::with_opts(opts!(
            "rgraph_cache_hit_rate",
            "Buffer pool cache hit rate (0.0–1.0)"
        ))
        .expect("INVARIANT: static metric opts are valid");
        registry
            .register(Box::new(cache_hit_rate.clone()))
            .expect("INVARIANT: fresh registry has no name collisions");

        Arc::new(Self {
            registry,
            queries_total,
            queries_failed,
            query_latency,
            active_connections,
            connections_opened,
            connections_closed,
            transactions_total,
            transactions_committed,
            transactions_aborted,
            cache_hits,
            cache_misses,
            cache_hit_rate,
        })
    }

    /// Borrow the per-server registry (e.g. to gather or expose it elsewhere).
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Record a query latency observation.
    pub fn observe_query_latency(&self,
        latency: Duration,
    ) {
        self.queries_total.inc();
        self.query_latency.observe(latency.as_secs_f64());
    }

    /// Record a failed query.
    pub fn observe_query_failed(&self) {
        self.queries_failed.inc();
    }

    /// Record that a transaction was begun.
    pub fn observe_transaction_begun(&self) {
        self.transactions_total.inc();
    }

    /// Record that a transaction committed successfully.
    pub fn observe_transaction_committed(&self) {
        self.transactions_committed.inc();
    }

    /// Record that a transaction aborted (explicit rollback or conflict).
    pub fn observe_transaction_aborted(&self) {
        self.transactions_aborted.inc();
    }

    /// Record that a client connection was opened.
    pub fn observe_connection_opened(&self) {
        self.connections_opened.inc();
        self.active_connections.inc();
    }

    /// Record that a client connection was closed.
    pub fn observe_connection_closed(&self) {
        self.connections_closed.inc();
        self.active_connections.dec();
    }

    /// Update the active connection gauge.
    pub fn set_active_connections(
        &self,
        n: i64,
    ) {
        self.active_connections.set(n as f64);
    }

    /// Update the cache hit rate gauge.
    pub fn set_cache_hit_rate(
        &self,
        rate: f64,
    ) {
        self.cache_hit_rate.set(rate.clamp(0.0, 1.0));
    }

    /// Record the absolute buffer-pool hit/miss counts and refresh the derived
    /// hit-rate gauge.
    ///
    /// The supplied totals are cumulative since pool creation; the counters are
    /// advanced by the delta against their current value so repeated calls do
    /// not double-count.
    pub fn record_cache_stats(&self, total_hits: u64, total_misses: u64) {
        let prev_hits = self.cache_hits.get() as u64;
        let prev_misses = self.cache_misses.get() as u64;
        if total_hits > prev_hits {
            self.cache_hits.inc_by((total_hits - prev_hits) as f64);
        }
        if total_misses > prev_misses {
            self.cache_misses.inc_by((total_misses - prev_misses) as f64);
        }
        let total = total_hits + total_misses;
        if total > 0 {
            self.cache_hit_rate
                .set((total_hits as f64 / total as f64).clamp(0.0, 1.0));
        }
    }

    /// Render all metrics in Prometheus text exposition format from this
    /// server's dedicated registry.
    ///
    /// # Errors
    ///
    /// Returns an [`std::fmt::Error`] if encoding fails (this is effectively
    /// unreachable for in-memory string encoding but is propagated rather than
    /// panicked to keep the serving path panic-free).
    pub fn render_prometheus(&self) -> Result<String, std::fmt::Error> {
        let encoder = TextEncoder::new();
        let families = self.registry.gather();
        let mut buffer = String::new();
        encoder
            .encode_utf8(&families, &mut buffer)
            .map_err(|_| std::fmt::Error)?;
        Ok(buffer)
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        // Build a standalone collector with its own registry, then unwrap the
        // Arc so callers that want an owned value (mostly tests) can have one.
        Arc::try_unwrap(MetricsCollector::new())
            .map_err(|_| ())
            .expect("INVARIANT: freshly created Arc has a unique owner")
    }
}

/// Health-service logic shared between gRPC and optional HTTP endpoint.
pub struct HealthService {
    ready: RwLock<bool>,
}

impl Default for HealthService {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthService {
    pub fn new() -> Self {
        Self {
            ready: RwLock::new(true),
        }
    }

    pub async fn is_ready(&self) -> bool {
        *self.ready.read().await
    }

    pub async fn set_ready(&self,
        ready: bool,
    ) {
        let mut guard = self.ready.write().await;
        *guard = ready;
    }
}

/// HTTP handler that serves `/metrics` in Prometheus text format.
///
/// Prefer [`MetricsCollector::render_prometheus`] for the per-server registry;
/// this helper renders the process-global default registry and is retained for
/// any non-server metrics that may still register there.
pub struct MetricsExporter;

impl MetricsExporter {
    /// Render the global Prometheus registry as a UTF-8 string.
    ///
    /// Returns an empty string if encoding fails rather than panicking.
    pub fn export() -> String {
        let encoder = TextEncoder::new();
        let families = prometheus::gather();
        let mut buffer = String::new();
        let _ = encoder.encode_utf8(&families, &mut buffer);
        buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_collector_records_latency() {
        let m = MetricsCollector::default();
        m.observe_query_latency(Duration::from_millis(5));
        assert_eq!(m.queries_total.get() as u64, 1);
    }

    #[test]
    fn metrics_collector_tracks_connections() {
        let m = MetricsCollector::default();
        m.set_active_connections(7);
        assert_eq!(m.active_connections.get() as i64, 7);
    }

    #[test]
    fn metrics_exporter_outputs_text() {
        // Ensure at least one metric is registered with the global registry.
        let counter = Counter::with_opts(opts!("test_export_metric", "test")).unwrap();
        let _ = prometheus::register(Box::new(counter));
        let text = MetricsExporter::export();
        assert!(!text.is_empty());
    }

    #[tokio::test]
    async fn health_service_toggles_ready() {
        let h = HealthService::new();
        assert!(h.is_ready().await);
        h.set_ready(false).await;
        assert!(!h.is_ready().await);
    }

    #[test]
    fn per_server_registries_are_isolated() {
        // Two collectors must not share state, and constructing many of them
        // must not fail with "already registered" (the global-registry bug).
        let a = MetricsCollector::new();
        let b = MetricsCollector::new();

        a.observe_transaction_committed();
        a.observe_transaction_committed();

        assert_eq!(a.transactions_committed.get() as u64, 2);
        assert_eq!(
            b.transactions_committed.get() as u64,
            0,
            "the second collector must be independent of the first"
        );
    }

    #[test]
    fn connection_metrics_track_open_and_close() {
        let m = MetricsCollector::new();
        m.observe_connection_opened();
        m.observe_connection_opened();
        m.observe_connection_closed();

        assert_eq!(m.connections_opened.get() as u64, 2);
        assert_eq!(m.connections_closed.get() as u64, 1);
        assert_eq!(m.active_connections.get() as i64, 1);
    }

    #[test]
    fn transaction_metrics_track_lifecycle() {
        let m = MetricsCollector::new();
        m.observe_transaction_begun();
        m.observe_transaction_committed();
        m.observe_transaction_aborted();

        assert_eq!(m.transactions_total.get() as u64, 1);
        assert_eq!(m.transactions_committed.get() as u64, 1);
        assert_eq!(m.transactions_aborted.get() as u64, 1);
    }

    #[test]
    fn cache_stats_update_counters_and_rate() {
        let m = MetricsCollector::new();
        m.record_cache_stats(75, 25);
        assert_eq!(m.cache_hits.get() as u64, 75);
        assert_eq!(m.cache_misses.get() as u64, 25);
        assert!((m.cache_hit_rate.get() - 0.75).abs() < 1e-9);

        // A later, larger cumulative reading advances by the delta only.
        m.record_cache_stats(100, 25);
        assert_eq!(m.cache_hits.get() as u64, 100);
        assert_eq!(m.cache_misses.get() as u64, 25);
    }

    #[test]
    fn render_prometheus_exposes_per_server_metrics() {
        let m = MetricsCollector::new();
        m.observe_query_failed();
        let text = m.render_prometheus().expect("render ok");
        assert!(
            text.contains("rgraph_queries_failed_total"),
            "per-server registry must expose the failure counter"
        );
    }
}
