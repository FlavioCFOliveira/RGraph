//! Metrics exporter and health-check endpoint (Task 54).
//!
//! Uses the [`prometheus`] crate to maintain counters, histograms, and gauges.
//! The [`HealthService`] gRPC handler and an optional HTTP `/health` endpoint
//! both read from the same [`MetricsCollector`] instance.

use prometheus::{
    gather, histogram_opts, opts, Counter, Gauge, Histogram, TextEncoder,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Central collector for all server metrics.
pub struct MetricsCollector {
    /// Total number of queries executed.
    pub queries_total: Counter,
    /// Total number of failed queries.
    pub queries_failed: Counter,
    /// Query latency histogram (seconds).
    pub query_latency: Histogram,
    /// Number of active connections.
    pub active_connections: Gauge,
    /// Total number of transactions.
    pub transactions_total: Counter,
    /// Buffer-pool hit rate (0.0–1.0).
    pub cache_hit_rate: Gauge,
}

impl MetricsCollector {
    /// Create a new collector and register all metrics with the global registry.
    pub fn new() -> Arc<Self> {
        let queries_total = Counter::with_opts(opts!(
            "rgraph_queries_total",
            "Total number of Cypher queries received"
        ))
        .expect("metric construction");
        let _ = prometheus::register(Box::new(queries_total.clone()));

        let queries_failed = Counter::with_opts(opts!(
            "rgraph_queries_failed_total",
            "Total number of Cypher queries that returned an error"
        ))
        .expect("metric construction");
        let _ = prometheus::register(Box::new(queries_failed.clone()));

        let query_latency = Histogram::with_opts(histogram_opts!(
            "rgraph_query_latency_seconds",
            "Query execution latency",
            vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
        ))
        .expect("metric construction");
        let _ = prometheus::register(Box::new(query_latency.clone()));

        let active_connections = Gauge::with_opts(opts!(
            "rgraph_active_connections",
            "Number of currently open client connections"
        ))
        .expect("metric construction");
        let _ = prometheus::register(Box::new(active_connections.clone()));

        let transactions_total = Counter::with_opts(opts!(
            "rgraph_transactions_total",
            "Total number of transactions begun"
        ))
        .expect("metric construction");
        let _ = prometheus::register(Box::new(transactions_total.clone()));

        let cache_hit_rate = Gauge::with_opts(opts!(
            "rgraph_cache_hit_rate",
            "Buffer pool cache hit rate (0.0–1.0)"
        ))
        .expect("metric construction");
        let _ = prometheus::register(Box::new(cache_hit_rate.clone()));

        Arc::new(Self {
            queries_total,
            queries_failed,
            query_latency,
            active_connections,
            transactions_total,
            cache_hit_rate,
        })
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

    /// Render all metrics in Prometheus text exposition format.
    pub fn render_prometheus(&self) -> String {
        let encoder = TextEncoder::new();
        let families = gather();
        let mut buffer = String::new();
        encoder.encode_utf8(&families, &mut buffer).unwrap();
        buffer
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        // Used only for tests; the real instance is created via `Arc::new(MetricsCollector::new())`.
        let queries_total = Counter::with_opts(opts!("test_queries", "test")).unwrap();
        let queries_failed = Counter::with_opts(opts!("test_failed", "test")).unwrap();
        let query_latency = Histogram::with_opts(histogram_opts!("test_latency", "test", vec![])).unwrap();
        let active_connections = Gauge::with_opts(opts!("test_conns", "test")).unwrap();
        let transactions_total = Counter::with_opts(opts!("test_txns", "test")).unwrap();
        let cache_hit_rate = Gauge::with_opts(opts!("test_cache", "test")).unwrap();
        Self {
            queries_total,
            queries_failed,
            query_latency,
            active_connections,
            transactions_total,
            cache_hit_rate,
        }
    }
}

/// Health-service logic shared between gRPC and optional HTTP endpoint.
pub struct HealthService {
    ready: RwLock<bool>,
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
pub struct MetricsExporter;

impl MetricsExporter {
    /// Render the global Prometheus registry as a UTF-8 string.
    pub fn export() -> String {
        let encoder = TextEncoder::new();
        let families = gather();
        let mut buffer = String::new();
        encoder.encode_utf8(&families, &mut buffer).unwrap();
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
}
