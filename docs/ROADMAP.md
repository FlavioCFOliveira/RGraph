# RGraph Master Roadmap

## Overview

RGraph is a greenfield Rust graph database designed for production-grade, high-concurrency deployments supporting both **Label Property Graph (LPG)** and **RDF** models with **100% openCypher** and **100% ACID** compliance. This roadmap synthesizes five specialized audit reports (architecture, cypher, storage, concurrency, api_ux) into a sequenced, six-sprint plan.

## Architecture

The engine follows a layered architecture:

```
┌─────────────────────────────────────────────┐
│  API & Protocol Layer                       │
│  (Cypher / gRPC / Programmatic API)         │
├─────────────────────────────────────────────┤
│  Query Layer                                │
│  Parser → Planner → Optimizer → Executor    │
├─────────────────────────────────────────────┤
│  Dual Model Layer                           │
│  LPG View (Nodes, Edges, Properties)        │
│  RDF View  (Triples, Quads, IRIs)           │
├─────────────────────────────────────────────┤
│  Transaction & Concurrency Layer            │
│  WAL → MVCC → Lock Manager → Recovery       │
├─────────────────────────────────────────────┤
│  Index Layer                                │
│  B-Tree │ Hash │ Full-Text │ Range │ Adj    │
├─────────────────────────────────────────────┤
│  Storage Engine Abstraction                 │
│  RocksDB / heed / Custom                    │
│  (Column Families / KV batches)             │
├─────────────────────────────────────────────┤
│  I/O & Async Runtime                        │
│  tokio / File I/O / Memory Mapping          │
└─────────────────────────────────────────────┘
```

### LPG and RDF Coexistence Strategy

A unified physical representation based on a generalized quad store is used internally. LPG is the native physical model optimized for traversal and property access. RDF is exposed as a logical projection via an adapter layer that translates IRIs, literals, and blank nodes into the internal schema, optionally providing SPARQL-to-Cypher translation. Both models are ACID-safe under a single transaction manager.

## Technology Choices

| Technology | Purpose |
|------------|---------|
| **Tokio** | Async runtime for server mode and cooperative multitasking |
| **RocksDB** | Production-grade LSM-tree storage backend with WAL and column families |
| **Heed** | Type-safe LMDB wrapper for read-heavy fallback environments |
| **Logos** | Fast zero-copy lexer for the Cypher frontend |
| **Rowan** | Green Tree / CST library for error-resilient syntax trees |
| **Parking Lot** | High-performance synchronization primitives |
| **Crossbeam Epoch** | Lock-free memory reclamation for concurrent indexes |
| **Tantivy** | Full-text search engine for text property indexing |
| **Sophia API** | RDF trait ecosystem for conformance validation |
| **UUID v7** | Stable, K-sortable identifiers |
| **Thiserror** | Structured error derivation for the crate-wide `Error` enum |
| **Proptest** | Property-based testing for ACID and parser correctness |
| **Criterion** | Statistical benchmarking for performance regression detection |

## Sprints

### Sprint 1: Foundation & CLI Vertical Slice
**Goal:** Deliver a runnable CLI binary that can initialize a graph database and execute basic Cypher expression queries (`RETURN`, literals, arithmetic). Establish CI, error handling, logging, documentation skeleton, and wire the TCK harness.

**Themes:** Tooling & CI, Storage Abstraction, Cypher Parser Bootstrap, API Surface, TCK Harness Wiring

| Title | Type | Priority |
|-------|------|----------|
| Set up CI pipeline (Formatting, Linting, Security Audit) | CHORE | 9 |
| Define crate-wide Error enum and Result alias | TASK | 9 |
| Create GraphBuilder and Config types | TASK | 9 |
| Implement ID allocation and serialization primitives | TASK | 9 |
| Design storage engine async trait (backend-agnostic API) | TASK | 9 |
| Integrate RocksDB as primary embedded backend | TASK | 9 |
| Implement Transaction lifecycle (begin, commit, rollback) | TASK | 9 |
| Implement logos-based lexer for all Cypher token classes | TASK | 9 |
| Implement Pratt expression parser for all Cypher operators and precedence levels | TASK | 9 |
| Design AST/CST node hierarchy with source spans | TASK | 9 |
| Build TCK Cucumber harness in Rust | TASK | 9 |
| Implement naive interpreter for expression-only queries | TASK | 8 |
| Create CLI init and query commands | USER_STORY | 8 |
| Set up tracing/logging infrastructure | TASK | 8 |
| Create mdbook skeleton and rustdoc structure | CHORE | 7 |

### Sprint 2: Graph Data Model & Basic Read Queries
**Goal:** Implement full LPG primitive support (Node, Relationship, Property) and execute fixed-length `MATCH` patterns, `WHERE`, `ORDER BY`, `SKIP`, `LIMIT` against a real graph stored in RocksDB.

**Themes:** LPG Storage, Cypher Read Path, Indexing Basics, API Hardening

| Title | Type | Priority |
|-------|------|----------|
| Build LPG adjacency and property storage layer | TASK | 9 |
| Implement Node and Relationship builder patterns | TASK | 9 |
| Add Property enum with From conversions | TASK | 9 |
| Implement label index for fast MATCH (n:Label) | TASK | 8 |
| Implement fixed-length pattern matcher and naive interpreter | TASK | 8 |
| Build semantic analyzer framework (scope resolution and type checking) | TASK | 9 |
| Implement property secondary indexes | TASK | 8 |
| Map error taxonomy to TCK error types and phases | TASK | 8 |
| Write first integration test with temporary database fixture | TASK | 8 |
| Update Knowledge Graph with storage schema and parser design | CHORE | 7 |

### Sprint 3: Variable-Length Paths & Graph Mutations
**Goal:** Support variable-length patterns with path restrictors and implement all write clauses (`CREATE`, `DELETE`, `SET`, `REMOVE`, `MERGE`) with correct eager isolation and side-effect tracking.

**Themes:** Advanced Pattern Matching, Graph Mutations, Write Isolation, TCK Write Compliance

| Title | Type | Priority |
|-------|------|----------|
| Implement variable-length pattern expander with configurable path restrictors | TASK | 9 |
| Implement graph mutations (CREATE, DELETE, SET, REMOVE) | TASK | 9 |
| Implement MERGE clause match-or-create semantics | TASK | 9 |
| Implement eager execution isolation for updates | TASK | 9 |
| Implement side-effect tracker matching TCK triple semantics | TASK | 8 |
| Implement UNWIND and list/map expression deep evaluation | TASK | 8 |
| Implement WITH clause and basic subquery scoping | TASK | 8 |
| Implement pattern matching with non-local predicates | TASK | 8 |
| Benchmark basic read/write throughput | SPIKE | 7 |

### Sprint 4: Aggregation, Subqueries & Query Planner
**Goal:** Replace the naive interpreter with a logical query planner and physical execution engine. Support aggregations, implicit grouping, subqueries, and `UNION` with correct Cypher semantics.

**Themes:** Query Planner, Aggregation, Subqueries, Performance Foundation

| Title | Type | Priority |
|-------|------|----------|
| Implement aggregation engine with implicit grouping | TASK | 9 |
| Implement subquery engine (EXISTS, COUNT, COLLECT) | TASK | 9 |
| Implement UNION and CALL for built-in procedures | TASK | 8 |
| Build logical query planner | TASK | 8 |
| Build physical execution engine with iterator-based operators | TASK | 8 |
| Implement query optimizer (predicate pushdown and index selection) | TASK | 8 |
| Implement catalog / schema manager | TASK | 8 |
| Implement constraint manager (uniqueness and existence) | TASK | 7 |
| Harden semantic analyzer for all TCK error types | TASK | 8 |
| Run TCK aggregation and subquery features | USER_STORY | 9 |

### Sprint 5: Durability, MVCC & Server Mode
**Goal:** Harden ACID guarantees with a custom WAL, MVCC snapshot isolation, and group commit. Bootstrap the async server on Tokio with gRPC, request dispatching, CPU worker pools, and backpressure.

**Themes:** ACID Hardening, Async Server, Protocol, Production Safety

| Title | Type | Priority |
|-------|------|----------|
| Implement WAL and group-commit durability layer | TASK | 9 |
| Implement MVCC snapshot isolation core | TASK | 9 |
| Implement strict lock ordering and deadlock prevention | TASK | 8 |
| Implement async checkpointing and background compaction scheduling | TASK | 7 |
| Implement async runtime bootstrap (Tokio) with graceful shutdown | TASK | 9 |
| Build request dispatcher and CPU-bound worker pool offload | TASK | 9 |
| Define gRPC service schema and implement RPC handlers | TASK | 8 |
| Implement connection acceptor and protocol multiplexer | TASK | 9 |
| Implement backpressure and connection limits | TASK | 8 |
| Implement metrics exporter and health check endpoint | TASK | 7 |
| Implement lock-free graph index (crossbeam-epoch) | TASK | 9 |
| Run ACID stress tests and recovery validation | USER_STORY | 9 |

### Sprint 6: Advanced Features, RDF & Production Hardening
**Goal:** Project RDF over the unified LPG storage, harden the query planner with cost-based optimization, complete CLI commands, and push to 100% openCypher TCK compliance with full documentation.

**Themes:** RDF Support, Query Optimization, Production Readiness, Documentation

| Title | Type | Priority |
|-------|------|----------|
| Prototype RDF triple/quad store projection over LPG storage | TASK | 8 |
| Implement cache-friendly adjacency layout (CSR + SOA) with freeze/thaw | TASK | 8 |
| Integrate full-text search index (tantivy) | TASK | 7 |
| Implement spatial index (R-tree / Hilbert curve) | TASK | 7 |
| Implement cost-based query planner (cardinality estimation and join reordering) | TASK | 7 |
| Implement server-mode authentication and multi-tenancy isolation | TASK | 6 |
| Complete CLI commands (serve, import, export, benchmark) | USER_STORY | 8 |
| Complete openCypher TCK compliance push | USER_STORY | 9 |
| Write mdbook User Guide and Architecture Decision Records | CHORE | 7 |
| Establish benchmark regression pipeline | CHORE | 7 |

## Critical Path

The critical path (tasks that must be completed in sequence and directly block downstream work) is:

1. Set up CI pipeline
2. Define crate-wide Error enum and Result alias
3. Design storage engine async trait
4. Implement ID allocation and serialization primitives
5. Create GraphBuilder and Config types
6. Integrate RocksDB as primary embedded backend
7. Implement Transaction lifecycle
8. Implement logos-based lexer
9. Implement Pratt expression parser
10. Design AST/CST node hierarchy
11. Build TCK Cucumber harness
12. Build LPG adjacency and property storage layer
13. Implement Node and Relationship builder patterns
14. Implement label index
15. Implement fixed-length pattern matcher
16. Build semantic analyzer framework
17. Implement variable-length pattern expander
18. Implement graph mutations
19. Implement MERGE clause
20. Implement eager execution isolation
21. Implement aggregation engine
22. Implement subquery engine
23. Build logical query planner
24. Build physical execution engine
25. Implement WAL and group-commit
26. Implement MVCC snapshot isolation
27. Implement async runtime bootstrap (Tokio)
28. Build request dispatcher and CPU worker pool offload
29. Define gRPC service schema
30. Prototype RDF triple/quad store projection
31. Complete openCypher TCK compliance push

## Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| openCypher TCK Incompleteness | High | Adopt hand-written parser (logos + Pratt). Build custom TCK test runner in CI. Track grammar coverage. |
| ACID Overhead Degrades Throughput | High | Use MVCC with snapshot isolation. Keep hot topology in memory. Batch reads. Benchmark against LDBC SNB. |
| Dual Model (LPG + RDF) Complexity | Medium | Unified physical quad-like representation. LPG native; RDF logical projection. Single transaction manager. |
| RocksDB FFI Build Friction | Medium | Provide pure-Rust storage trait so RocksDB is one backend. Evaluate heed (LMDB) as fallback. Containerize builds. |
| Deadlocks and Lock Contention | High | Favor MVCC and optimistic locking. Use parking_lot and crossbeam. Implement deadlock detection and timeout/abort. |
| Storage Engine Choice Lock-in | Medium | Abstract storage behind a trait (StorageEngine, Transaction, Cursor). Enables future migration. |
| Memory Pressure with Large Graphs | Medium | Design for memory-mapped or block-buffered access. Use arena allocators for temporary query state. |

## Immediate Next Steps

1. Initialize the Cargo workspace and commit the initial project structure including `rustfmt.toml`, `clippy.toml`, and a GitHub Actions CI workflow.
2. Open the first `rmp` task ticket for the crate-wide Error enum, assign it to Sprint 1, and mark it as the current active task.
3. Draft ADR-001 documenting the Storage Engine Abstraction trait boundaries, the RocksDB primary backend choice, and the LMDB fallback rationale.
4. Add `logos`, `rowan`, `thiserror`, and `rocksdb` to `Cargo.toml` and scaffold the `lexer` and `parser` modules with initial token definitions.
5. Set up the TCK harness skeleton using the `cucumber` crate and wire a single passing step definition for `Given an empty graph`.

## Knowledge Graph

The project Knowledge Graph is maintained in `rmp` (Groadmap) and tracks:

- **Sprints** and their tasks
- **Components** (Unified Graph Data Model, Graph Topology Engine, Storage Engine Abstraction, Write-Ahead Log, Transaction Manager, Index Manager, Cypher Query Parser, Query Planner, Query Execution Engine, RDF/SPARQL Adapter, Server & Protocol Layer, Memory & Cache Manager)
- **Technologies** (Tokio, RocksDB, Heed, Logos, Rowan, Parking Lot, Crossbeam Epoch, Tantivy, Sophia API, UUID, Thiserror, Proptest, Criterion)
- **Requirements** (100% openCypher Compliance, 100% ACID Compliance, LPG Support, RDF Support, Server Mode, One-Shot Mode)
- **Risks** and their mitigations
- **Documents** (CLAUDE.md, Architecture Decision Records, User Guide, API Documentation, openCypher Compliance Matrix)

Relationships include `DEPENDS_ON` between components, `REQUIRES` between requirements and components, `MITIGATED_BY` between risks and components/tasks, and `SCHEDULED_IN` between tasks and sprints.

---

*This roadmap was generated on 2026-06-02 and is managed via the `rmp` (Groadmap) CLI under the `rgraph` roadmap.*
