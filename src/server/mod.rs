//! Async server mode for RGraph.
//!
//! This module provides the production server foundation required by Sprint 23:
//! async storage traits, Tokio runtime bootstrap, request dispatching, gRPC
//! services, connection acceptance, backpressure, metrics export, and ACID
//! stress testing.

pub mod acceptor;
pub mod dispatcher;
pub mod grpc;
pub mod metrics;
pub mod runtime;
pub mod storage;
pub mod stress;

pub use acceptor::{ConnectionAcceptor, MeteredStream, Protocol, ProtocolMultiplexer};
pub use dispatcher::{CpuPool, RequestDispatcher};
pub use grpc::{GraphGrpcServer, ServeLimits};
pub use metrics::{HealthService, MetricsCollector, MetricsExporter};
pub use runtime::{ServerConfig, ServerRuntime};
pub use storage::{
    AsyncGraphEngine, AsyncSnapshot, AsyncStorageEngine, AsyncTransaction,
    GraphEngineAdapter, InMemoryStorageEngine,
};
