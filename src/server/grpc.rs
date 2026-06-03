//! gRPC service handlers (Task 51).
//!
//! Implements [`CypherQuery`](proto::cypher_query_server), [`TransactionManager`]
//! and [`Health`](proto::health_server) services using [`tonic`].
//!
//! The query evaluator is intentionally minimal: it handles `RETURN` with
//! literals and arithmetic so that the acceptance criterion "a basic query
/// RPC executes a RETURN statement end-to-end" is satisfied.  Full execution
/// (MATCH, CREATE, etc.) depends on Sprint 21 (Query Execution Engine).

use crate::cypher::ast::{
    BinaryOperator, Clause, ComparisonOperator, Expression, Literal, Statement, UnaryOperator,
};
use crate::cypher::parser::parse;
use crate::error::RGraphError;
use crate::server::storage::AsyncGraphEngine;
use crate::server::metrics::MetricsCollector;
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

    /// Minimal evaluator for `RETURN` statements with literals and arithmetic.
    fn evaluate_return(statement: &Statement) -> Result<ResultSet, RGraphError> {
        let mut columns = Vec::new();
        let mut row_values = Vec::new();

        for clause in &statement.clauses {
            if let Clause::Return(ret) = clause {
                for proj in &ret.projections {
                    columns.push(proj.alias.clone().unwrap_or_else(|| "column".into()));
                    let val = Self::eval_expression(&proj.expression)?;
                    row_values.push(val);
                }
            }
        }

        let row = Row { values: row_values };
        Ok(ResultSet {
            columns,
            rows: vec![row],
        })
    }

    fn eval_expression(expr: &Expression) -> Result<ProtoValue, RGraphError> {
        match expr {
            Expression::Literal(Literal::Integer(i)) => Ok(ProtoValue {
                kind: Some(proto::value::Kind::Integer(*i)),
            }),
            Expression::Literal(Literal::String(s)) => Ok(ProtoValue {
                kind: Some(proto::value::Kind::String(s.clone())),
            }),
            Expression::Literal(Literal::Float(f)) => Ok(ProtoValue {
                kind: Some(proto::value::Kind::Float(*f)),
            }),
            Expression::Literal(Literal::Boolean(b)) => Ok(ProtoValue {
                kind: Some(proto::value::Kind::Boolean(*b)),
            }),
            Expression::Literal(Literal::Null) => Ok(ProtoValue {
                kind: Some(proto::value::Kind::Null(proto::Null {})),
            }),
            Expression::BinaryOp { op, left, right } => {
                let lhs = Self::eval_expression(left)?;
                let rhs = Self::eval_expression(right)?;
                Self::eval_binary_op(lhs, op, rhs)
            }
            Expression::Comparison { op, left, right } => {
                let lhs = Self::eval_expression(left)?;
                let rhs = Self::eval_expression(right)?;
                Self::eval_comparison(lhs, op, rhs)
            }
            Expression::UnaryOp { op, expr } => {
                let val = Self::eval_expression(expr)?;
                Self::eval_unary_op(op, val)
            }
            Expression::Variable(name) => Err(RGraphError::Semantic(format!(
                "variable '{}' not bound (execution engine not yet implemented)",
                name
            ))),
            Expression::PropertyAccess { base: _, property } => Err(RGraphError::Semantic(format!(
                "property access .{} not supported (execution engine not yet implemented)",
                property
            ))),
            Expression::List(items) => {
                let mut values = Vec::new();
                for item in items {
                    values.push(Self::eval_expression(item)?);
                }
                Ok(ProtoValue {
                    kind: Some(proto::value::Kind::List(proto::ListValue { values })),
                })
            }
            Expression::Map(entries) => {
                let mut map = std::collections::HashMap::new();
                for (k, v) in entries {
                    map.insert(k.clone(), Self::eval_expression(v)?);
                }
                Ok(ProtoValue {
                    kind: Some(proto::value::Kind::Map(proto::MapValue { entries: map })),
                })
            }
            Expression::IsNull(_) | Expression::IsNotNull(_) => Err(RGraphError::Semantic(
                "IS NULL / IS NOT NULL not yet implemented".into(),
            )),
        }
    }

    fn eval_binary_op(
        lhs: ProtoValue,
        op: &BinaryOperator,
        rhs: ProtoValue,
    ) -> Result<ProtoValue, RGraphError> {
        use proto::value::Kind;
        let l = lhs.kind.ok_or_else(|| RGraphError::Type("empty lhs".into()))?;
        let r = rhs.kind.ok_or_else(|| RGraphError::Type("empty rhs".into()))?;

        match (l, r) {
            (Kind::Integer(a), Kind::Integer(b)) => {
                let res = match op {
                    BinaryOperator::Add => a + b,
                    BinaryOperator::Sub => a - b,
                    BinaryOperator::Mul => a * b,
                    BinaryOperator::Div => a / b,
                    BinaryOperator::Mod => a % b,
                    BinaryOperator::Pow => a.pow(b as u32),
                };
                Ok(ProtoValue { kind: Some(Kind::Integer(res)) })
            }
            (Kind::Float(a), Kind::Float(b)) => {
                let res = match op {
                    BinaryOperator::Add => a + b,
                    BinaryOperator::Sub => a - b,
                    BinaryOperator::Mul => a * b,
                    BinaryOperator::Div => a / b,
                    _ => return Err(RGraphError::Type("unsupported float op".into())),
                };
                Ok(ProtoValue { kind: Some(Kind::Float(res)) })
            }
            (Kind::String(a), Kind::String(b)) => {
                let res = match op {
                    BinaryOperator::Add => format!("{}{}", a, b),
                    _ => return Err(RGraphError::Type("unsupported string op".into())),
                };
                Ok(ProtoValue { kind: Some(Kind::String(res)) })
            }
            _ => Err(RGraphError::Type(format!(
                "type mismatch in binary expression"
            ))),
        }
    }

    fn eval_comparison(
        lhs: ProtoValue,
        op: &ComparisonOperator,
        rhs: ProtoValue,
    ) -> Result<ProtoValue, RGraphError> {
        use proto::value::Kind;
        let l = lhs.kind.ok_or_else(|| RGraphError::Type("empty lhs".into()))?;
        let r = rhs.kind.ok_or_else(|| RGraphError::Type("empty rhs".into()))?;

        let res = match (l, r) {
            (Kind::Integer(a), Kind::Integer(b)) => match op {
                ComparisonOperator::Eq => a == b,
                ComparisonOperator::Ne => a != b,
                ComparisonOperator::Lt => a < b,
                ComparisonOperator::Le => a <= b,
                ComparisonOperator::Gt => a > b,
                ComparisonOperator::Ge => a >= b,
            },
            (Kind::Float(a), Kind::Float(b)) => match op {
                ComparisonOperator::Eq => a == b,
                ComparisonOperator::Ne => a != b,
                ComparisonOperator::Lt => a < b,
                ComparisonOperator::Le => a <= b,
                ComparisonOperator::Gt => a > b,
                ComparisonOperator::Ge => a >= b,
            },
            (Kind::String(a), Kind::String(b)) => match op {
                ComparisonOperator::Eq => a == b,
                ComparisonOperator::Ne => a != b,
                _ => return Err(RGraphError::Type("unsupported string comparison".into())),
            },
            (Kind::Boolean(a), Kind::Boolean(b)) => match op {
                ComparisonOperator::Eq => a == b,
                ComparisonOperator::Ne => a != b,
                _ => return Err(RGraphError::Type("unsupported bool comparison".into())),
            },
            _ => return Err(RGraphError::Type("type mismatch in comparison".into())),
        };
        Ok(ProtoValue {
            kind: Some(Kind::Boolean(res)),
        })
    }

    fn eval_unary_op(op: &UnaryOperator, val: ProtoValue) -> Result<ProtoValue, RGraphError> {
        use proto::value::Kind;
        let v = val.kind.ok_or_else(|| RGraphError::Type("empty operand".into()))?;
        match (op, v) {
            (UnaryOperator::Neg, Kind::Integer(i)) => Ok(ProtoValue {
                kind: Some(Kind::Integer(-i)),
            }),
            (UnaryOperator::Neg, Kind::Float(f)) => Ok(ProtoValue {
                kind: Some(Kind::Float(-f)),
            }),
            (UnaryOperator::Not, Kind::Boolean(b)) => Ok(ProtoValue {
                kind: Some(Kind::Boolean(!b)),
            }),
            _ => Err(RGraphError::Type(format!("unsupported unary op: {:?}", op))),
        }
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

        let result = async {
            let statement = parse(&req.query)?;

            // For Sprint 23 we only support RETURN (minimal evaluator).
            if statement.clauses.iter().any(|c| matches!(c, Clause::Match(_))) {
                return Err(RGraphError::Semantic(
                    "MATCH not yet implemented (Sprint 21)".into(),
                ));
            }
            if statement.clauses.iter().any(|c| matches!(c, Clause::Create(_))) {
                return Err(RGraphError::Semantic(
                    "CREATE not yet implemented (Sprint 21)".into(),
                ));
            }

            let result_set = Self::evaluate_return(&statement)?;
            Ok(QueryResponse {
                result: Some(proto::query_response::Result::ResultSet(result_set)),
            })
        }
        .await;

        let latency = start.elapsed();
        self.metrics.observe_query_latency(latency);

        match result {
            Ok(resp) => Ok(Response::new(resp)),
            Err(e) => {
                warn!("query execution failed: {}", e);
                Ok(Response::new(QueryResponse {
                    result: Some(proto::query_response::Result::Error(ErrorPayload {
                        code: "RGraphError".into(),
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

pub struct TransactionService;

#[tonic::async_trait]
impl proto::transaction_manager_server::TransactionManager for TransactionService {
    async fn begin(
        &self,
        _request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        // Sprint 23: stub — full MVCC integration is part of Sprint 21/22.
        Ok(Response::new(BeginResponse {
            result: Some(proto::begin_response::Result::TxId(0)),
        }))
    }

    async fn commit(
        &self,
        _request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        Ok(Response::new(CommitResponse {
            result: Some(proto::commit_response::Result::Ok(true)),
        }))
    }

    async fn rollback(
        &self,
        _request: Request<RollbackRequest>,
    ) -> Result<Response<RollbackResponse>, Status> {
        Ok(Response::new(RollbackResponse {
            result: Some(proto::rollback_response::Result::Ok(true)),
        }))
    }
}

// ------------------------------------------------------------------
// Health service
// ------------------------------------------------------------------

pub struct HealthServiceImpl;

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
        
        let encoder = prometheus::TextEncoder::new();
        let metric_families = prometheus::gather();
        let mut buffer = String::new();
        encoder.encode_utf8(&metric_families, &mut buffer).unwrap();
        Ok(Response::new(MetricsResponse {
            prometheus_text: buffer,
        }))
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
            engine, metrics,
        ));
        let tx = proto::transaction_manager_server::TransactionManagerServer::new(
            TransactionService,
        );
        let health = proto::health_server::HealthServer::new(HealthServiceImpl);

        tonic::transport::Server::builder()
            .add_service(cypher)
            .add_service(tx)
            .add_service(health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_return_literal_integer() {
        let stmt = parse("RETURN 42").unwrap();
        let result = CypherQueryService::evaluate_return(&stmt).unwrap();
        assert_eq!(result.columns, vec!["column"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values[0].kind,
            Some(proto::value::Kind::Integer(42))
        );
    }

    #[test]
    fn evaluate_return_arithmetic() {
        let stmt = parse("RETURN 1 + 2 AS sum").unwrap();
        let result = CypherQueryService::evaluate_return(&stmt).unwrap();
        assert_eq!(result.columns, vec!["sum"]);
        assert_eq!(
            result.rows[0].values[0].kind,
            Some(proto::value::Kind::Integer(3))
        );
    }

    #[test]
    fn evaluate_return_string_concat() {
        let stmt = parse("RETURN 'hello' + 'world' AS greeting").unwrap();
        let result = CypherQueryService::evaluate_return(&stmt).unwrap();
        assert_eq!(result.columns, vec!["greeting"]);
        assert_eq!(
            result.rows[0].values[0].kind,
            Some(proto::value::Kind::String("helloworld".into()))
        );
    }

    #[test]
    fn evaluate_return_boolean_comparison() {
        let stmt = parse("RETURN 1 = 1 AS eq").unwrap();
        let result = CypherQueryService::evaluate_return(&stmt).unwrap();
        assert_eq!(result.columns, vec!["eq"]);
        assert_eq!(
            result.rows[0].values[0].kind,
            Some(proto::value::Kind::Boolean(true))
        );
    }
}
