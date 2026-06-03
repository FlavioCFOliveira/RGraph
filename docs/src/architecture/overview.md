# Architecture Overview

RGraph is organised into layered subsystems, each with a single
responsibility and well-defined interfaces.

## Layer Diagram

```text
┌─────────────────────────────────────────────┐
│  Server Layer    (gRPC, Bolt, metrics)      │
├─────────────────────────────────────────────┤
│  Query Layer     (parser, AST, planner)     │
├─────────────────────────────────────────────┤
│  Graph API       (Node, Relationship,       │
│                  Property, scan, index)     │
├─────────────────────────────────────────────┤
│  Storage Engine  (B+ tree, slotted pages,  │
│                  buffer pool, WAL)           │
├─────────────────────────────────────────────┤
│  I/O Runtime     (POSIX, async bridge,     │
│                  scheduler)                  │
└─────────────────────────────────────────────┘
```

## Design Principles

1. **Zero-cost abstractions** — The graph API compiles down to direct
   page-manager calls; there is no ORM or heavyweight runtime.
2. **Fail-fast durability** — Every write is WAL-logged before the
   in-memory page is marked dirty.  `fsync` is explicit and traceable.
3. **No hidden allocations** — Hot paths (B+ tree traversal, lock
   acquisition) use pre-allocated structures and lock-free algorithms
   where possible.
4. **Observability by default** — Every subsystem emits structured
   `tracing` spans and Prometheus metrics.

## Module Map

| Module | File | Responsibility |
|--------|------|--------------|
| `server` | `src/server/` | Async runtime, gRPC, metrics, stress tests |
| `cypher` | `src/cypher/` | Parser, AST, semantic analyser |
| `graph` | `src/graph/` | High-level graph API and record formats |
| `index` | `src/index/` | B+ tree core, label/type/property indexes |
| `buffer` | `src/buffer/` | CLOCK-Pro buffer pool and background flusher |
| `storage` | `src/storage/` | Page manager, bitmap, superblock |
| `wal` | `src/wal/` | WAL writer, ARIES recovery, group commit |
| `txn` | `src/txn/` | MVCC, lock table, wound-wait, transaction manager |
| `io` | `src/io/` | Aligned buffers, POSIX filesystem abstraction |
