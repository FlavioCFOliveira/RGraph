# Changelog

All notable changes to RGraph are documented in this file.

## [Unreleased]

### Added
- Sprint 23: Server Mode Foundation
  - Async storage engine traits (`AsyncStorageEngine`, `AsyncTransaction`,
    `AsyncSnapshot`, `AsyncGraphEngine`)
  - In-memory mock backend for testing
  - Tokio runtime bootstrap with graceful shutdown
  - gRPC service schema and handlers (CypherQuery, TransactionManager, Health)
  - TCP/TLS connection acceptor and protocol multiplexer
  - Prometheus metrics exporter and health endpoints
  - ACID stress test harness
  - ADR-001: Async Storage Engine Traits
- Sprint 22: Conformidade Gates
  - Cypher TCK compliance test harness
  - ACID compliance test suite (Atomicity, Consistency, Isolation, Durability)
- Sprint 8: Graph Data Model and Secondary Indexes
  - Property enum and builders
  - Graph API with label/type/property index scans
  - Cypher parser, AST, and semantic analyser

### Fixed
- Cypher parser now supports arithmetic operators (`+`, `-`, `*`, `/`, `%`)

## [0.1.0] — 2026-06-03

### Added
- Initial project structure and storage foundation
- Page manager with superblock and bitmap
- B+ tree core with latch crabbing and optimistic reads
- Buffer pool with CLOCK-Pro replacement
- WAL writer and ARIES recovery
- MVCC tuple headers and snapshot isolation
- Sharded lock table with wound-wait deadlock prevention
- Graph record formats (Node, Edge, Property)
- Label and type secondary indexes
- Property secondary index with prefix compression
