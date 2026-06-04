//! gRPC service handlers.
//!
//! Implements the `CypherQuery`, `TransactionManager` and `Health` services
//! using [`tonic`].
//!
//! `CypherQuery::Execute` routes each request through the full Sprint C query
//! pipeline (parse → semantic analyse → plan → physical execute) against the
//! shared graph engine.  `TransactionManager` is backed by the engine's MVCC
//! [`TransactionManager`](crate::txn::manager::TransactionManager): `Begin`
//! allocates a real snapshot-bearing transaction tracked server-side by its
//! `tx_id`, and `Commit`/`Rollback` finalise it through the engine's WAL.

use crate::cypher::value::Value as CypherValue;
use crate::graph::property::OrderedF64;
use crate::error::RGraphError;
use crate::server::storage::{AsyncGraphEngine, CypherResult};
use crate::server::metrics::MetricsCollector;
use std::collections::HashMap;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{debug, warn};

pub mod proto {
    tonic::include_proto!("rgraph");
}

use proto::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse,
    ErrorPayload, HealthCheckRequest, HealthCheckResponse,
    MetricsRequest, MetricsResponse, QueryRequest, QueryResponse,
    ReadinessRequest, ReadinessResponse, ResultSet, RollbackRequest,
    RollbackResponse, Row, Value as ProtoValue,
};

// ------------------------------------------------------------------
// Error mapping: RGraphError → gRPC error payloads
// ------------------------------------------------------------------

/// A short, stable machine-readable label for an [`RGraphError`] variant,
/// embedded in [`ErrorPayload::code`] so clients can branch on the class of
/// failure without parsing the human-readable message.
fn error_code_label(e: &RGraphError) -> &'static str {
    match e {
        RGraphError::Syntax(_) => "SYNTAX",
        RGraphError::Semantic(_) => "SEMANTIC",
        RGraphError::Type(_) => "TYPE",
        RGraphError::Argument(_) => "ARGUMENT",
        RGraphError::NotFound(_) => "NOT_FOUND",
        RGraphError::AlreadyExists(_) => "ALREADY_EXISTS",
        RGraphError::Io(_) => "IO",
        RGraphError::Corruption(_) => "CORRUPTION",
        RGraphError::Index(_) => "INDEX",
        RGraphError::Storage(_) => "STORAGE",
        RGraphError::Transaction(_) => "TRANSACTION",
        RGraphError::ResourceExhausted(_) => "RESOURCE_EXHAUSTED",
        RGraphError::Internal(_) => "INTERNAL",
    }
}

// ------------------------------------------------------------------
// Value conversion: Cypher Value <-> proto Value
// ------------------------------------------------------------------

/// Convert a runtime Cypher [`CypherValue`] into the wire [`ProtoValue`].
fn cypher_to_proto(value: CypherValue) -> ProtoValue {
    use proto::value::Kind;
    let kind = match value {
        CypherValue::Null => Kind::Null(proto::Null {}),
        CypherValue::Boolean(b) => Kind::Boolean(b),
        CypherValue::Integer(i) => Kind::Integer(i),
        CypherValue::Float(f) => Kind::Float(f.0),
        CypherValue::String(s) => Kind::String(s),
        CypherValue::List(items) => Kind::List(proto::ListValue {
            values: items.into_iter().map(cypher_to_proto).collect(),
        }),
        CypherValue::Map(entries) => Kind::Map(proto::MapValue {
            entries: entries
                .into_iter()
                .map(|(k, v)| (k, cypher_to_proto(v)))
                .collect(),
        }),
        CypherValue::Node(node) => Kind::Node(proto::NodeValue {
            node_id: node.id,
            label: node.labels.first().cloned().unwrap_or_default(),
            properties: node
                .properties
                .into_iter()
                .map(|(k, v)| (k, cypher_to_proto(v)))
                .collect(),
        }),
        CypherValue::Relationship(rel) => Kind::Relationship(proto::RelationshipValue {
            edge_id: rel.id,
            rel_type: rel.rel_type,
            source_id: rel.source_id,
            target_id: rel.target_id,
            properties: rel
                .properties
                .into_iter()
                .map(|(k, v)| (k, cypher_to_proto(v)))
                .collect(),
        }),
        // Paths and points have no dedicated wire representation yet; render
        // them as their textual form so the result is still observable.
        other => Kind::String(other.to_string()),
    };
    ProtoValue { kind: Some(kind) }
}

/// Convert a wire [`ProtoValue`] into a runtime Cypher [`CypherValue`].
///
/// Used to translate the `parameters` map that accompanies a query request.
/// Node/relationship wire values are not valid query parameters and map to
/// [`CypherValue::Null`].
fn proto_to_cypher(value: ProtoValue) -> CypherValue {
    use proto::value::Kind;
    match value.kind {
        None | Some(Kind::Null(_)) => CypherValue::Null,
        Some(Kind::Boolean(b)) => CypherValue::Boolean(b),
        Some(Kind::Integer(i)) => CypherValue::Integer(i),
        Some(Kind::Float(f)) => CypherValue::Float(OrderedF64(f)),
        Some(Kind::String(s)) => CypherValue::String(s),
        Some(Kind::Bytes(b)) => {
            // No first-class byte value; expose as a string of the UTF-8 lossy form.
            CypherValue::String(String::from_utf8_lossy(&b).into_owned())
        }
        Some(Kind::List(list)) => {
            CypherValue::List(list.values.into_iter().map(proto_to_cypher).collect())
        }
        Some(Kind::Map(map)) => CypherValue::Map(
            map.entries
                .into_iter()
                .map(|(k, v)| (k, proto_to_cypher(v)))
                .collect(),
        ),
        Some(Kind::Node(_)) | Some(Kind::Relationship(_)) => CypherValue::Null,
    }
}

/// Convert a [`CypherResult`] into a wire [`ResultSet`].
fn result_to_proto(result: CypherResult) -> ResultSet {
    let columns = result.columns;
    let rows = result
        .rows
        .into_iter()
        .map(|row| Row {
            values: row.into_iter().map(cypher_to_proto).collect(),
        })
        .collect();
    ResultSet { columns, rows }
}

// ------------------------------------------------------------------
// Cypher query service
// ------------------------------------------------------------------

pub struct CypherQueryService {
    engine: Arc<dyn AsyncGraphEngine>,
    metrics: Arc<MetricsCollector>,
}

impl CypherQueryService {
    pub fn new(engine: Arc<dyn AsyncGraphEngine>, metrics: Arc<MetricsCollector>) -> Self {
        Self { engine, metrics }
    }
}

#[tonic::async_trait]
impl proto::cypher_query_server::CypherQuery for CypherQueryService {
    async fn execute(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let req = request.into_inner();
        let start = std::time::Instant::now();
        debug!("execute query: {}", req.query);

        // Translate the wire parameter map into runtime Cypher values.
        let parameters: HashMap<String, CypherValue> = req
            .parameters
            .into_iter()
            .map(|(k, v)| (k, proto_to_cypher(v)))
            .collect();

        // Route through the real Sprint C pipeline against the shared engine.
        let result = self.engine.execute_cypher(req.query, parameters).await;

        let latency = start.elapsed();
        self.metrics.observe_query_latency(latency);

        // Refresh buffer-pool cache metrics from the engine's live counters so
        // dashboards reflect the I/O this query performed.
        if let Some((hits, misses)) = self.engine.cache_stats().await {
            self.metrics.record_cache_stats(hits, misses);
        }

        match result {
            Ok(cypher_result) => {
                let result_set = result_to_proto(cypher_result);
                Ok(Response::new(QueryResponse {
                    result: Some(proto::query_response::Result::ResultSet(result_set)),
                }))
            }
            Err(e) => {
                warn!("query execution failed: {}", e);
                self.metrics.observe_query_failed();
                Ok(Response::new(QueryResponse {
                    result: Some(proto::query_response::Result::Error(ErrorPayload {
                        code: error_code_label(&e).into(),
                        message: e.to_string(),
                    })),
                }))
            }
        }
    }

    type ExecuteStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<QueryResponse, Status>>;

    async fn execute_stream(
        &self,
        request: Request<tonic::Streaming<QueryRequest>>,
    ) -> Result<Response<Self::ExecuteStreamStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let engine = self.engine.clone();
        let metrics = self.metrics.clone();

        tokio::spawn(async move {
            while let Some(req) = stream.message().await.transpose() {
                let req = match req {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        continue;
                    }
                };
                // Reuse the single-query logic.
                let service = CypherQueryService::new(engine.clone(), metrics.clone());
                let fake_req = Request::new(req);
                match service.execute(fake_req).await {
                    Ok(resp) => {
                        let _ = tx.send(Ok(resp.into_inner())).await;
                    }
                    Err(e) => {
                        let _ = tx.send(Ok(QueryResponse {
                            result: Some(proto::query_response::Result::Error(ErrorPayload {
                                code: "RGraphError".into(),
                                message: e.to_string(),
                            })),
                        }))
                        .await;
                    }
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

// ------------------------------------------------------------------
// Transaction service
// ------------------------------------------------------------------

/// gRPC transaction service backed by the engine's MVCC
/// [`TransactionManager`](crate::txn::manager::TransactionManager).
///
/// Each `Begin` allocates a real transaction (and snapshot) tracked
/// server-side by its `tx_id`; `Commit`/`Rollback` finalise it through the
/// engine's WAL so durability and isolation are honoured.
pub struct TransactionService {
    engine: Arc<dyn AsyncGraphEngine>,
    metrics: Arc<MetricsCollector>,
}

impl TransactionService {
    pub fn new(engine: Arc<dyn AsyncGraphEngine>, metrics: Arc<MetricsCollector>) -> Self {
        Self { engine, metrics }
    }
}

#[tonic::async_trait]
impl proto::transaction_manager_server::TransactionManager for TransactionService {
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        let req = request.into_inner();
        match self.engine.begin_txn(req.read_only).await {
            Ok(txid) => {
                self.metrics.observe_transaction_begun();
                Ok(Response::new(BeginResponse {
                    result: Some(proto::begin_response::Result::TxId(txid)),
                }))
            }
            Err(e) => {
                warn!("begin transaction failed: {}", e);
                Ok(Response::new(BeginResponse {
                    result: Some(proto::begin_response::Result::Error(ErrorPayload {
                        code: error_code_label(&e).into(),
                        message: e.to_string(),
                    })),
                }))
            }
        }
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        let req = request.into_inner();
        match self.engine.commit_txn(req.tx_id).await {
            Ok(()) => {
                self.metrics.observe_transaction_committed();
                Ok(Response::new(CommitResponse {
                    result: Some(proto::commit_response::Result::Ok(true)),
                }))
            }
            Err(RGraphError::NotFound(msg)) => Err(Status::not_found(msg)),
            Err(e) => {
                warn!("commit transaction failed: {}", e);
                self.metrics.observe_transaction_aborted();
                Ok(Response::new(CommitResponse {
                    result: Some(proto::commit_response::Result::Error(ErrorPayload {
                        code: error_code_label(&e).into(),
                        message: e.to_string(),
                    })),
                }))
            }
        }
    }

    async fn rollback(
        &self,
        request: Request<RollbackRequest>,
    ) -> Result<Response<RollbackResponse>, Status> {
        let req = request.into_inner();
        match self.engine.rollback_txn(req.tx_id).await {
            Ok(()) => {
                self.metrics.observe_transaction_aborted();
                Ok(Response::new(RollbackResponse {
                    result: Some(proto::rollback_response::Result::Ok(true)),
                }))
            }
            Err(RGraphError::NotFound(msg)) => Err(Status::not_found(msg)),
            Err(e) => {
                warn!("rollback transaction failed: {}", e);
                Ok(Response::new(RollbackResponse {
                    result: Some(proto::rollback_response::Result::Error(ErrorPayload {
                        code: error_code_label(&e).into(),
                        message: e.to_string(),
                    })),
                }))
            }
        }
    }
}

// ------------------------------------------------------------------
// Health service
// ------------------------------------------------------------------

pub struct HealthServiceImpl {
    metrics: Arc<MetricsCollector>,
}

impl HealthServiceImpl {
    pub fn new(metrics: Arc<MetricsCollector>) -> Self {
        Self { metrics }
    }
}

#[tonic::async_trait]
impl proto::health_server::Health for HealthServiceImpl {
    async fn check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        Ok(Response::new(HealthCheckResponse {
            status: proto::health_check_response::ServingStatus::Serving as i32,
        }))
    }

    async fn ready(
        &self,
        _request: Request<ReadinessRequest>,
    ) -> Result<Response<ReadinessResponse>, Status> {
        Ok(Response::new(ReadinessResponse { ready: true }))
    }

    async fn metrics(
        &self,
        _request: Request<MetricsRequest>,
    ) -> Result<Response<MetricsResponse>, Status> {
        // Render this server's dedicated registry; fall back to an empty body
        // on the (effectively unreachable) encode failure rather than panicking.
        let prometheus_text = self
            .metrics
            .render_prometheus()
            .map_err(|e| Status::internal(format!("metrics encode failed: {e}")))?;
        Ok(Response::new(MetricsResponse { prometheus_text }))
    }
}

// ------------------------------------------------------------------
// Server builder
// ------------------------------------------------------------------

/// Backpressure and deadline limits applied to the tonic transport.
#[derive(Debug, Clone, Copy)]
pub struct ServeLimits {
    /// Per-request deadline; requests exceeding it receive
    /// `Status::deadline_exceeded`.
    pub request_timeout: std::time::Duration,
    /// Maximum number of concurrent in-flight requests per connection.
    pub concurrency_per_connection: usize,
    /// Maximum number of concurrent in-flight requests across all connections.
    pub global_concurrency: usize,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            request_timeout: std::time::Duration::from_secs(30),
            concurrency_per_connection: 256,
            global_concurrency: 4096,
        }
    }
}

/// Bundles all gRPC services into a single [`tonic::transport::Server`].
pub struct GraphGrpcServer;

impl GraphGrpcServer {
    /// Build the default router (no transport limits applied).
    pub fn routes(
        engine: Arc<dyn AsyncGraphEngine>,
        metrics: Arc<MetricsCollector>,
    ) -> tonic::transport::server::Router {
        Self::routes_with_limits(engine, metrics, None)
    }

    /// Build the router, optionally applying tonic's request-timeout and
    /// per-connection concurrency limit (both leave the router's layer stack
    /// unchanged, so the return type is the plain [`Router`]).
    ///
    /// The cross-connection global concurrency limit is applied separately in
    /// [`serve_with_acceptor`](GraphGrpcServer::serve_with_acceptor) because it
    /// is a tower layer that alters the service type.
    pub fn routes_with_limits(
        engine: Arc<dyn AsyncGraphEngine>,
        metrics: Arc<MetricsCollector>,
        limits: Option<ServeLimits>,
    ) -> tonic::transport::server::Router {
        let (cypher, tx, health) = Self::build_services(engine, metrics);

        let mut builder = tonic::transport::Server::builder();
        if let Some(limits) = limits {
            builder = builder
                .timeout(limits.request_timeout)
                .concurrency_limit_per_connection(limits.concurrency_per_connection);
        }

        builder
            .add_service(cypher)
            .add_service(tx)
            .add_service(health)
    }

    /// Construct the three gRPC service handlers sharing `engine`/`metrics`.
    #[allow(clippy::type_complexity)]
    fn build_services(
        engine: Arc<dyn AsyncGraphEngine>,
        metrics: Arc<MetricsCollector>,
    ) -> (
        proto::cypher_query_server::CypherQueryServer<CypherQueryService>,
        proto::transaction_manager_server::TransactionManagerServer<TransactionService>,
        proto::health_server::HealthServer<HealthServiceImpl>,
    ) {
        let cypher = proto::cypher_query_server::CypherQueryServer::new(CypherQueryService::new(
            engine.clone(),
            metrics.clone(),
        ));
        let tx = proto::transaction_manager_server::TransactionManagerServer::new(
            TransactionService::new(engine, metrics.clone()),
        );
        let health = proto::health_server::HealthServer::new(HealthServiceImpl::new(metrics));
        (cypher, tx, health)
    }

    /// Serve the gRPC services over `acceptor`'s incoming connection stream
    /// until `shutdown` resolves, then drain in-flight requests gracefully.
    ///
    /// This is the production serving path: the [`ConnectionAcceptor`] enforces
    /// the global connection limit, performs TLS, and multiplexes the protocol;
    /// the applied [`ServeLimits`] add a request deadline, a per-connection
    /// concurrency cap, and a global concurrency-limit tower layer; and
    /// `serve_with_incoming_shutdown` performs a graceful stop, refusing new
    /// connections while letting active requests complete.
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::Io`] if the transport fails fatally.
    pub async fn serve_with_acceptor<F>(
        engine: Arc<dyn AsyncGraphEngine>,
        metrics: Arc<MetricsCollector>,
        acceptor: crate::server::acceptor::ConnectionAcceptor,
        limits: ServeLimits,
        shutdown: F,
    ) -> Result<(), RGraphError>
    where
        F: std::future::Future<Output = ()> + Send,
    {
        let (cypher, tx, health) = Self::build_services(engine, metrics.clone());

        // Clamp the global concurrency limit to tokio's permit ceiling.
        const MAX_GLOBAL: usize = 1 << 24;
        let global_limit = limits.global_concurrency.clamp(1, MAX_GLOBAL);

        let router = tonic::transport::Server::builder()
            .timeout(limits.request_timeout)
            .concurrency_limit_per_connection(limits.concurrency_per_connection)
            .layer(tower::limit::GlobalConcurrencyLimitLayer::new(global_limit))
            .add_service(cypher)
            .add_service(tx)
            .add_service(health);

        let incoming = acceptor.into_incoming(metrics);

        router
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await
            .map_err(|e| RGraphError::Io(format!("grpc serve failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::storage::GraphEngineAdapter;
    use proto::cypher_query_server::CypherQuery;
    use proto::transaction_manager_server::TransactionManager as _;

    /// Build a [`CypherQueryService`] backed by a fresh on-disk engine.
    fn cypher_service() -> (tempfile::TempDir, CypherQueryService) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();
        (dir, CypherQueryService::new(engine, metrics))
    }

    fn run_query(svc: &CypherQueryService, query: &str) -> QueryResponse {
        let resp = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(svc.execute(Request::new(QueryRequest {
                query: query.to_string(),
                parameters: HashMap::new(),
            })))
            .unwrap();
        resp.into_inner()
    }

    #[test]
    fn execute_return_literal_through_pipeline() {
        let (_dir, svc) = cypher_service();
        let resp = run_query(&svc, "RETURN 42 AS answer");
        match resp.result {
            Some(proto::query_response::Result::ResultSet(rs)) => {
                assert_eq!(rs.columns, vec!["answer"]);
                assert_eq!(rs.rows.len(), 1);
                assert_eq!(
                    rs.rows[0].values[0].kind,
                    Some(proto::value::Kind::Integer(42))
                );
            }
            other => panic!("expected result set, got {other:?}"),
        }
    }

    #[test]
    fn execute_match_returns_seeded_rows() {
        let (_dir, svc) = cypher_service();
        // Seed three nodes via the real pipeline (CREATE), then MATCH them back.
        let _ = run_query(&svc, "CREATE (:Person {name: 'Alice'})");
        let _ = run_query(&svc, "CREATE (:Person {name: 'Bob'})");
        let _ = run_query(&svc, "CREATE (:Person {name: 'Carol'})");

        let resp = run_query(&svc, "MATCH (n) RETURN n");
        match resp.result {
            Some(proto::query_response::Result::ResultSet(rs)) => {
                assert_eq!(
                    rs.rows.len(),
                    3,
                    "expected 3 seeded nodes, got {}",
                    rs.rows.len()
                );
            }
            other => panic!("expected result set, got {other:?}"),
        }
    }

    #[test]
    fn execute_syntax_error_returns_error_payload() {
        let (_dir, svc) = cypher_service();
        let resp = run_query(&svc, "RETURN ((((");
        match resp.result {
            Some(proto::query_response::Result::Error(err)) => {
                assert_eq!(err.code, "SYNTAX");
            }
            other => panic!("expected error payload, got {other:?}"),
        }
    }

    #[test]
    fn parameters_are_threaded_into_execution() {
        let (_dir, svc) = cypher_service();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut params = HashMap::new();
        params.insert(
            "x".to_string(),
            ProtoValue {
                kind: Some(proto::value::Kind::Integer(7)),
            },
        );
        let resp = rt
            .block_on(svc.execute(Request::new(QueryRequest {
                query: "RETURN $x AS x".to_string(),
                parameters: params,
            })))
            .unwrap()
            .into_inner();
        match resp.result {
            Some(proto::query_response::Result::ResultSet(rs)) => {
                assert_eq!(
                    rs.rows[0].values[0].kind,
                    Some(proto::value::Kind::Integer(7))
                );
            }
            other => panic!("expected result set, got {other:?}"),
        }
    }

    #[test]
    fn transaction_begin_commit_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();
        let svc = TransactionService::new(engine, metrics);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Begin → returns a non-zero tx id.
        let begin = rt
            .block_on(svc.begin(Request::new(BeginRequest { read_only: false })))
            .unwrap()
            .into_inner();
        let txid = match begin.result {
            Some(proto::begin_response::Result::TxId(id)) => id,
            other => panic!("expected tx id, got {other:?}"),
        };
        assert!(txid > 0, "tx id must be non-zero");

        // Commit → ok.
        let commit = rt
            .block_on(svc.commit(Request::new(CommitRequest { tx_id: txid })))
            .unwrap()
            .into_inner();
        assert!(matches!(
            commit.result,
            Some(proto::commit_response::Result::Ok(true))
        ));
    }

    #[test]
    fn transaction_rollback_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();
        let svc = TransactionService::new(engine, metrics);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let begin = rt
            .block_on(svc.begin(Request::new(BeginRequest { read_only: false })))
            .unwrap()
            .into_inner();
        let txid = match begin.result {
            Some(proto::begin_response::Result::TxId(id)) => id,
            other => panic!("expected tx id, got {other:?}"),
        };

        let rollback = rt
            .block_on(svc.rollback(Request::new(RollbackRequest { tx_id: txid })))
            .unwrap()
            .into_inner();
        assert!(matches!(
            rollback.result,
            Some(proto::rollback_response::Result::Ok(true))
        ));
    }

    #[test]
    fn commit_unknown_tx_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();
        let svc = TransactionService::new(engine, metrics);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let err = rt
            .block_on(svc.commit(Request::new(CommitRequest { tx_id: 999_999 })))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    // ------------------------------------------------------------------
    // Observability metric tests (Task 178)
    // ------------------------------------------------------------------

    #[test]
    fn query_error_increments_failure_metric() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();
        let svc = CypherQueryService::new(engine, metrics.clone());

        // Drive a syntactically invalid query.
        let _ = run_query(&svc, "RETURN ((((");

        assert!(
            metrics.queries_failed.get() as u64 >= 1,
            "a failed query must increment rgraph_queries_failed_total"
        );
    }

    #[test]
    fn transaction_rpcs_increment_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();
        let svc = TransactionService::new(engine, metrics.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();

        let begin = rt
            .block_on(svc.begin(Request::new(BeginRequest { read_only: false })))
            .unwrap()
            .into_inner();
        let txid = match begin.result {
            Some(proto::begin_response::Result::TxId(id)) => id,
            other => panic!("expected tx id, got {other:?}"),
        };
        rt.block_on(svc.commit(Request::new(CommitRequest { tx_id: txid })))
            .unwrap();

        assert_eq!(metrics.transactions_total.get() as u64, 1);
        assert_eq!(metrics.transactions_committed.get() as u64, 1);
    }

    // ------------------------------------------------------------------
    // Serving-path integration tests (Task 182)
    //
    // These bind a real listener on 127.0.0.1:0 and drive a tonic client.
    // Every client call is wrapped in a short timeout so a regression cannot
    // hang the test suite.
    // ------------------------------------------------------------------

    use crate::server::acceptor::ConnectionAcceptor;
    use std::time::Duration;

    const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

    fn test_limits() -> ServeLimits {
        ServeLimits {
            request_timeout: Duration::from_secs(5),
            concurrency_per_connection: 64,
            global_concurrency: 256,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_served_through_acceptor_path_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();

        let acceptor = ConnectionAcceptor::bind("127.0.0.1:0", None, None, 16)
            .await
            .unwrap();
        let port = acceptor.local_port;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(GraphGrpcServer::serve_with_acceptor(
            engine,
            metrics.clone(),
            acceptor,
            test_limits(),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // Connect a real client and issue a query through the acceptor path.
        let endpoint = format!("http://127.0.0.1:{port}");
        let mut client = tokio::time::timeout(CLIENT_TIMEOUT, async {
            // Retry connect briefly while the listener spins up.
            loop {
                match proto::cypher_query_client::CypherQueryClient::connect(endpoint.clone()).await
                {
                    Ok(c) => break c,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("client connected within timeout");

        let resp = tokio::time::timeout(
            CLIENT_TIMEOUT,
            client.execute(Request::new(QueryRequest {
                query: "RETURN 1 AS one".to_string(),
                parameters: HashMap::new(),
            })),
        )
        .await
        .expect("query returned within timeout")
        .expect("query ok")
        .into_inner();

        match resp.result {
            Some(proto::query_response::Result::ResultSet(rs)) => {
                assert_eq!(
                    rs.rows[0].values[0].kind,
                    Some(proto::value::Kind::Integer(1))
                );
            }
            other => panic!("expected result set, got {other:?}"),
        }

        // At least one connection was accounted for.
        assert!(metrics.connections_opened.get() >= 1.0);

        // Graceful shutdown.
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(CLIENT_TIMEOUT, server)
            .await
            .expect("server task joined")
            .expect("server join ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_limit_rejects_beyond_capacity() {
        use tokio::io::AsyncWriteExt;

        // A capacity-1 acceptor: the first connection holds the only permit, so
        // a second concurrent connection cannot be accepted.
        let acceptor = ConnectionAcceptor::bind("127.0.0.1:0", None, None, 1)
            .await
            .unwrap();
        let port = acceptor.local_port;

        // Connect and send the HTTP/2 preface so protocol detection completes
        // (the multiplexer peeks the first bytes before returning the stream).
        let mut c1 = tokio::time::timeout(
            CLIENT_TIMEOUT,
            tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")),
        )
        .await
        .expect("connect within timeout")
        .expect("first connection");
        c1.write_all(b"PRI * HTTP/2.0\r\n").await.unwrap();

        // Accept the first connection; this consumes the only permit.
        let first = tokio::time::timeout(CLIENT_TIMEOUT, acceptor.accept())
            .await
            .expect("first accept within timeout")
            .unwrap();
        assert!(first.is_some());
        assert_eq!(acceptor.available_permits(), 0);

        // A second accept must not complete while the permit is held.
        let second = tokio::time::timeout(Duration::from_millis(150), acceptor.accept()).await;
        assert!(
            second.is_err(),
            "second accept must block while the connection limit is saturated"
        );

        // Releasing the held connection frees the permit again.
        drop(first);
        assert_eq!(acceptor.available_permits(), 1);
        drop(c1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_stops_accepting() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rgraph.db");
        let engine: Arc<dyn AsyncGraphEngine> =
            Arc::new(GraphEngineAdapter::init(db).expect("init engine"));
        let metrics = MetricsCollector::new();

        let acceptor = ConnectionAcceptor::bind("127.0.0.1:0", None, None, 16)
            .await
            .unwrap();
        let port = acceptor.local_port;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(GraphGrpcServer::serve_with_acceptor(
            engine,
            metrics,
            acceptor,
            test_limits(),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // Make one successful request so the server is definitely up.
        let endpoint = format!("http://127.0.0.1:{port}");
        let mut client = tokio::time::timeout(CLIENT_TIMEOUT, async {
            loop {
                match proto::cypher_query_client::CypherQueryClient::connect(endpoint.clone()).await
                {
                    Ok(c) => break c,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("client connected");

        let _ = tokio::time::timeout(
            CLIENT_TIMEOUT,
            client.execute(Request::new(QueryRequest {
                query: "RETURN 1".to_string(),
                parameters: HashMap::new(),
            })),
        )
        .await
        .expect("in-flight request completed before shutdown")
        .expect("request ok");

        // Trigger graceful shutdown; the server task must finish promptly.
        let _ = shutdown_tx.send(());
        let joined = tokio::time::timeout(CLIENT_TIMEOUT, server)
            .await
            .expect("server stopped within timeout")
            .expect("server join ok");
        assert!(joined.is_ok(), "server returned cleanly: {joined:?}");
    }
}
