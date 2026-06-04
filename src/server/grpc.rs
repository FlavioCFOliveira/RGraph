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

/// Bundles all gRPC services into a single [`tonic::transport::Server`].
pub struct GraphGrpcServer;

impl GraphGrpcServer {
    pub fn routes(
        engine: Arc<dyn AsyncGraphEngine>,
        metrics: Arc<MetricsCollector>,
    ) -> tonic::transport::server::Router {
        let cypher = proto::cypher_query_server::CypherQueryServer::new(CypherQueryService::new(
            engine.clone(),
            metrics.clone(),
        ));
        let tx = proto::transaction_manager_server::TransactionManagerServer::new(
            TransactionService::new(engine, metrics.clone()),
        );
        let health = proto::health_server::HealthServer::new(HealthServiceImpl::new(metrics));

        tonic::transport::Server::builder()
            .add_service(cypher)
            .add_service(tx)
            .add_service(health)
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
}
