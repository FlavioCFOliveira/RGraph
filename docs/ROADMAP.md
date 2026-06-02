# RGraph Master Roadmap

## Overview

RGraph is a greenfield Rust graph database designed for production-grade, high-concurrency deployments supporting both **Label Property Graph (LPG)** and **RDF** models with **100% openCypher** and **100% ACID** compliance. This roadmap reflects the decision to build a **custom, graph-native storage engine** rather than relying on an external backend like RocksDB.

## Architecture

The engine follows a layered architecture with a fully custom storage stack:

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
│  RDF View  (Triples, Quads, IRIs)          │
├─────────────────────────────────────────────┤
│  Transaction & Concurrency Layer            │
│  MVCC → Snapshot Isolation → Lock Manager  │
├─────────────────────────────────────────────┤
│  Index Layer                                │
│  B+ Tree: Label │ Type │ Property │ Adj    │
│  B+ Tree: RDF SPO │ POS │ OSP          │
├─────────────────────────────────────────────┤
│  Page & Record Layer                        │
│  Slotted 8KB Pages → Node (32B) / Edge    │
│  (48B) / Property (variable, overflow)      │
├─────────────────────────────────────────────┤
│  Buffer Pool & I/O Layer                  │
│  CLOCK-Pro │ Sharded Page Table │ Background│
│  Flusher │ Fuzzy Checkpoint │ O_DIRECT     │
├─────────────────────────────────────────────┤
│  WAL & Recovery Layer                       │
│  ARIES: Analysis → Redo → Undo │ Group    │
│  Commit │ Double-Write Buffer              │
├─────────────────────────────────────────────┤
│  I/O Runtime Layer                          │
│  FileSystem Trait │ pread/pwrite │ io_uring│
│  (Linux) │ Blocking Pool (fallback)        │
└─────────────────────────────────────────────┘
```

### LPG and RDF Coexistence Strategy

A unified physical representation based on a generalized quad store is used internally. LPG is the native physical model optimized for traversal and property access. RDF is exposed as a logical projection via an adapter layer that translates IRIs, literals, and blank nodes into the internal schema, optionally providing SPARQL-to-Cypher translation. Both models are ACID-safe under a single transaction manager.

### Custom Storage Engine Design

The storage engine is organized into five layers:

1. **I/O Layer** — A trait-based abstraction over `pread`/`pwrite` with `O_DIRECT` alignment, backed by `io_uring` on Linux and a blocking thread pool on other platforms. A multi-queue weighted-fair scheduler ensures WAL appends and syncs (P0) never queue behind bulk reads (P3).

2. **WAL Layer** — Append-only segmented log with 64 MB rotation, group commit batching up to 64 transactions, `fdatasync` coalescing, and ARIES physical redo/undo records with Compensation Log Records (CLRs).

3. **Buffer Pool Layer** — Sharded hash-map page table, CLOCK-Pro with scan-resistant admission, atomic pin/dirty/clock bits per frame, background flushers, and strict WAL-before-data eviction.

4. **Page & Index Layer** — 8KB slotted pages with slot directories, prefix compression, and B+ trees for all indexes (label, type, property, adjacency, RDF SPO/POS/OSP). Structural changes use latch crabbing with ascending `page_id` ordering and `crossbeam-epoch` for lock-free page pointer updates.

5. **Transaction & Graph Layer** — Monotonic TxID allocator, in-page version chains with tuple headers (`xmin`/`xmax`/`CID`), snapshot acquisition at `BEGIN`, visibility evaluator, and integration of fixed-size 32-byte node and 48-byte edge records with doubly-linked adjacency lists.

## Technology Choices

| Technology | Purpose |
|------------|---------|
| **Tokio** | Async runtime for server mode and cooperative multitasking |
| **Logos** | Fast zero-copy lexer for the Cypher frontend |
| **Rowan** | Green Tree / CST library for error-resilient syntax trees |
| **Parking Lot** | High-performance synchronization primitives |
| **Crossbeam Epoch** | Lock-free memory reclamation for concurrent indexes |
| **Crossbeam Channel** | Sync-async bridge for the storage engine |
| **Tantivy** | Full-text search engine for text property indexing |
| **Sophia API** | RDF trait ecosystem for conformance validation |
| **UUID v7** | Stable, K-sortable identifiers |
| **Thiserror** | Structured error derivation for the crate-wide `Error` enum |
| **Proptest** | Property-based testing for ACID and parser correctness |
| **Criterion** | Statistical benchmarking for performance regression detection |
| **crc32c** | Hardware-accelerated CRC32-C for page and WAL header checksums |
| **twox-hash** | xxHash64 for WAL payload integrity |
| **io-uring / tokio-uring** | True async I/O on Linux |
| **bumpalo** | Per-transaction bump arenas for temporary allocations |
| **rayon** | Data-parallel CPU work offload |
| **libc** | `O_DIRECT`, `mbind`, `sched_setaffinity` FFI |
| **memmap2** | Optional mmap fallback for read-only analytics |
| **loom / shuttle** | Deterministic concurrency testing |

### Removed Dependencies

- `rocksdb` — replaced by custom B+ tree, buffer pool, and WAL
- `librocksdb-sys` — no longer needed

## Sprints

### Sprint 1: Storage Foundation
**Goal:** Deliver a runnable CLI binary that can create a database, allocate and write 8KB pages, persist mutations through a WAL, and recover to a consistent state after a simulated crash.

**Duration:** 4 weeks | **Themes:** I/O abstraction, Page format, Basic WAL, Runnable MVP

| Title | Type | Priority |
|-------|------|----------|
| Define portable FileSystem trait | TASK | 9 |
| Implement aligned buffer allocator | TASK | 9 |
| Define 8KB slotted page format | TASK | 9 |
| Build simple page manager | TASK | 9 |
| Implement WAL record codec | TASK | 9 |
| Build append-only WAL writer | TASK | 9 |
| Implement basic redo-only crash recovery | TASK | 9 |
| Create runnable CLI MVP | USER_STORY | 8 |

### Sprint 2: Buffer Pool & I/O Runtime
**Goal:** Replace the simple page manager with a production user-space buffer pool (CLOCK-Pro, background flush, fuzzy checkpoints) and establish the async/sync runtime bridge.

**Duration:** 4 weeks | **Themes:** Buffer pool, Background flush, Async I/O bridge, Checkpointing

| Title | Type | Priority |
|-------|------|----------|
| Implement frame table and sharded page table | TASK | 9 |
| Implement CLOCK-Pro replacement policy | TASK | 9 |
| Build background dirty-page flusher | TASK | 9 |
| Implement fuzzy checkpoint protocol | TASK | 9 |
| Build channel-based I/O runtime bridge | TASK | 9 |
| Implement multi-queue I/O scheduler | TASK | 9 |
| Add O_DIRECT support | TASK | 8 |
| Integrate PageLSN tracking and WAL-before-flush ordering | TASK | 9 |

### Sprint 3: B+ Tree Core
**Goal:** Implement a persistent B+ tree with slotted pages, latch crabbing, split/merge, optimistic reads, and crossbeam-epoch reclamation.

**Duration:** 4 weeks | **Themes:** B+ tree persistence, Latch crabbing, Split and merge, Lock-free reads

| Title | Type | Priority |
|-------|------|----------|
| Implement B+ tree leaf and branch page formats | TASK | 9 |
| Define composite key encoding | TASK | 9 |
| Implement latch crabbing with deadlock-free ordering | TASK | 9 |
| Implement split and merge with physical WAL records | TASK | 9 |
| Integrate crossbeam-epoch for page pointer swizzling | TASK | 9 |
| Build optimistic reader traversal | TASK | 8 |
| Implement bottom-up bulk loader | TASK | 8 |

### Sprint 4: Graph Native Storage
**Goal:** Implement native graph record formats (nodes, edges, properties, adjacency lists) and basic secondary indexes on top of the B+ tree and page manager.

**Duration:** 4 weeks | **Themes:** Node and edge records, Adjacency lists, Property storage, Basic indexes

| Title | Type | Priority |
|-------|------|----------|
| Implement fixed-size 32-byte node record format | TASK | 9 |
| Implement fixed-size 48-byte edge record format | TASK | 9 |
| Build doubly linked adjacency lists | TASK | 8 |
| Implement variable-length property storage | TASK | 8 |
| Integrate graph CRUD into StorageEngine trait | TASK | 9 |
| Implement label index B+ tree | TASK | 9 |
| Implement type index B+ tree | TASK | 8 |

### Sprint 5: Transactions & MVCC
**Goal:** Implement ACID transactions with snapshot isolation, version chains, lock management, and wound-wait deadlock prevention.

**Duration:** 4 weeks | **Themes:** TxID allocation, Snapshot isolation, Lock table, Deadlock prevention

| Title | Type | Priority |
|-------|------|----------|
| Implement monotonic TxID allocator | TASK | 9 |
| Build global transaction state manager | TASK | 9 |
| Design slotted page tuple headers for MVCC | TASK | 9 |
| Implement snapshot acquisition and visibility evaluator | TASK | 9 |
| Implement sharded lock table with wait queues | TASK | 9 |
| Implement wound-wait deadlock prevention | TASK | 9 |
| Implement transaction state machine | TASK | 8 |

### Sprint 6: ARIES Recovery & WAL Hardening
**Goal:** Achieve full ARIES-style crash recovery with group commit, bounded recovery time, and production-grade durability testing.

**Duration:** 4 weeks | **Themes:** Group commit, Analysis-Redo-Undo, Torn page protection, WAL testing

| Title | Type | Priority |
|-------|------|----------|
| Implement group-commit queue with fsync coalescing | TASK | 9 |
| Implement ANALYSIS phase | TASK | 9 |
| Implement REDO phase | TASK | 9 |
| Implement UNDO phase with CLR generation | TASK | 9 |
| Build double-write buffer for torn page protection | TASK | 9 |
| Implement WAL segment rotation and archival | TASK | 7 |
| Write proptest property tests for WAL | TASK | 8 |

### Sprint 7: RDF & Secondary Indexes
**Goal:** Add full dual-model support (LPG + RDF) with property indexes, adjacency indexes, RDF triple indexes, prefix compression, and online maintenance.

**Duration:** 3 weeks | **Themes:** Property and adjacency indexes, RDF triple indexes, Prefix compression, Online defragmentation

| Title | Type | Priority |
|-------|------|----------|
| Implement property index B+ tree | TASK | 9 |
| Implement adjacency index B+ tree | TASK | 8 |
| Implement RDF SPO/POS/OSP B+ tree indexes | TASK | 7 |
| Add prefix compression codec | TASK | 8 |
| Build secondary index group-commit integration | TASK | 9 |
| Implement online page defragmentation | TASK | 6 |

### Sprint 8: Server Mode & Production Hardening
**Goal:** Enable high-concurrency server mode with io_uring, NUMA awareness, zero-copy guards, phantom prevention, and comprehensive production testing.

**Duration:** 4 weeks | **Themes:** io_uring integration, NUMA and zero-copy, Serializable isolation, Production testing

| Title | Type | Priority |
|-------|------|----------|
| Implement io_uring backend | TASK | 8 |
| Implement NUMA-aware frame allocator | TASK | 7 |
| Implement zero-copy PageHandle guards | TASK | 8 |
| Implement sequential readahead | TASK | 7 |
| Implement I/O sort-merge elevator batching | TASK | 6 |
| Implement SSI or next-key locking for phantom prevention | TASK | 7 |
| Build FaultInjectFileSystem and DeterministicIoUring tests | TASK | 8 |
| Implement ACID stress tests and benchmark suite | TASK | 8 |

## Critical Path

The critical path (tasks that must be completed in sequence and directly block downstream work) is:

1. Define portable FileSystem trait
2. Implement aligned buffer allocator
3. Define 8KB slotted page format
4. Implement WAL record codec
5. Build append-only WAL writer
6. Implement frame table and sharded page table
7. Integrate PageLSN tracking and WAL-before-flush ordering
8. Implement latch crabbing with deadlock-free ordering
9. Implement split and merge with physical WAL records
10. Implement fixed-size 32-byte node record format
11. Implement fixed-size 48-byte edge record format
12. Implement monotonic TxID allocator
13. Build global transaction state manager
14. Implement snapshot acquisition and visibility evaluator
15. Implement sharded lock table with wait queues
16. Implement group-commit queue with fsync coalescing
17. Implement ANALYSIS phase
18. Implement REDO phase
19. Implement UNDO phase with CLR generation
20. Build double-write buffer for torn page protection
21. Implement property index B+ tree
22. Implement RDF SPO/POS/OSP B+ tree indexes
23. Implement io_uring backend
24. Build FaultInjectFileSystem and DeterministicIoUring tests
25. Implement ACID stress tests and benchmark suite

## Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Achieving RocksDB-level crash recovery reliability without years of battle testing | High | Implement deterministic fault injection and ARIES recovery property tests from Sprint 1; run continuous crash-recovery CI with random kill points |
| Designing a deadlock-free or recoverable locking protocol for dense-graph edge updates under high concurrency | High | Use strict ascending page_id latch ordering and wound-wait victim selection; validate with loom/shuttle concurrency tests and synthetic deadlock generators |
| Implementing io_uring correctly and safely in Rust with a custom runtime; fallback to async std I/O may reduce performance | Medium | Abstract I/O behind the FileSystem trait so io_uring is swappable; maintain a fully functional blocking pool backend from day one |
| Balancing page size and record format for both small property-heavy LPG nodes and large RDF string literals | Medium | Use inline threshold of 256 bytes with overflow chains; benchmark both LPG and RDF workloads before finalizing prefix compression thresholds |
| WAL size and fsync latency under sustained write load in server mode; risk of checkpoint stalls | High | Group commit with 1 ms timeout and 64-transaction target; fuzzy checkpoints that never block writers; explicit backpressure when WAL segments exceed 16 GB |
| Correctness of B+ tree structural modifications (splits, merges, rebalancing) under multi-threaded access; subtle bugs can cause unrecoverable corruption | High | Log all structural changes as physical redo/undo records; validate tree invariants after every mutation in debug builds; use proptest to generate random split/merge sequences |
| Time required to pass ACID stress tests and TCK compliance; unknown interaction between graph semantics and storage isolation levels | High | Build ACID stress tests in parallel with the transaction manager from Sprint 5; map openCypher MATCH semantics to snapshot isolation visibility rules explicitly |
| Crossbeam-epoch memory reclamation latency causing buffer pool frame exhaustion under sustained write throughput | Medium | Align epoch granularity with the async scheduler tick; add an eager reclamation fast path when pin_count reaches zero and no readers are active |

## Immediate Next Steps

1. Create the `FileSystem` trait and aligned buffer allocator as the first commit.
2. Write a design document for the 8KB slotted page format and composite key encoding.
3. Set up CI benchmarks for I/O latency before any storage code is written.
4. Remove the `rocksdb` and `librocksdb-sys` crates from `Cargo.toml` and all module references.
5. Initialize the rmp Knowledge Graph with nodes for `PageManager`, `WALManager`, `BufferPool`, `BPlusTree`, `TransactionManager`, and relationships mapping dependencies.

## Knowledge Graph

The project Knowledge Graph is maintained in `rmp` (Groadmap) and tracks:

- **Sprints** and their tasks
- **Components** (Unified Graph Data Model, Graph Topology Engine, Storage Engine Abstraction, Write-Ahead Log, Transaction Manager, Index Manager, Cypher Query Parser, Query Planner, Query Execution Engine, RDF/SPARQL Adapter, Server & Protocol Layer, Memory & Cache Manager, PageManager, FrameTable, WALRecordCodec, WALWriter, NodeRecord, EdgeRecord, PropertyRecord, LabelIndex, TypeIndex, PropertyIndex, AdjacencyIndex, RDF_SPO_Index, RDF_POS_Index, RDF_OSP_Index, TxIDAllocator, ShardedLockTable, IOScheduler)
- **Technologies** (Tokio, Logos, Rowan, Parking Lot, Crossbeam Epoch, Crossbeam Channel, Tantivy, Sophia API, UUID, Thiserror, Proptest, Criterion, FileSystemTrait, AlignedBufferAllocator, SlottedPageFormat, ShardedPageTable, CLOCKPro, FuzzyCheckpoint, GroupCommitQueue, BPlusTree, LatchCrabbing, OptimisticRead, EpochPagePtr, CompositeKeyEncoding, PrefixCompression, BulkLoader, AdjacencyList, Snapshot, TupleHeader, WoundWait, SSIPhantomPrevention, IOBridge, FaultInjectFileSystem, DeterministicIoUring)
- **Requirements** (100% openCypher Compliance, 100% ACID Compliance, LPG Support, RDF Support, Server Mode, One-Shot Mode)
- **Risks** and their mitigations
- **Documents** (CLAUDE.md, Architecture Decision Records, User Guide, API Documentation, openCypher Compliance Matrix)

Relationships include `DEPENDS_ON` between components, `REQUIRES` between requirements and components, `MITIGATED_BY` between risks and components/tasks, `USES`, `MANAGES`, `INDEXED_BY`, `STORES_IN`, `PROTECTED_BY`, `RECLAIMS_VIA`, `ALLOCATES`, `TRACKS`, `PREVENTS_PHANTOMS_WITH`, `SCHEDULES_THROUGH`, `IMPLEMENTS`, and `SCHEDULED_IN` between tasks and sprints.

---

*This roadmap was generated on 2026-06-02 and is managed via the `rmp` (Groadmap) CLI under the `rgraph` roadmap.*
