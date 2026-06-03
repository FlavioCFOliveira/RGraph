# ADR-001: Async Storage Engine Traits

## Status

Accepted — implemented in Sprint 23 (Server Mode Foundation).

## Context

RGraph's native storage layer (`GraphStorageEngine`) is synchronous and tightly
coupled to the page manager, B+ tree indexes, and WAL.  Before we can expose the
database over an async network protocol (gRPC/Bolt) we need:

1. A backend-agnostic async API so the server code does not depend on concrete
   page-manager internals.
2. An in-memory mock backend for fast unit tests and CI.
3. A bridge from the existing sync engine to the async trait so we do not
   block the Tokio runtime during CPU-bound graph operations.

## Decision

We introduce three layers of abstraction:

| Layer | Trait | Purpose |
|-------|-------|---------|
| KV | `AsyncStorageEngine` | `get`, `put`, `delete`, `scan_prefix`, `begin_transaction`, `snapshot` |
| TX | `AsyncTransaction` / `AsyncSnapshot` | Isolated read-write and read-only views |
| Graph | `AsyncGraphEngine` | High-level graph operations consumed by gRPC handlers |

### Why `async-trait` instead of RPITIT everywhere?

Rust 1.75+ supports RPITIT (Return Position `impl Trait` In Traits), which lets
us write:

```rust
trait AsyncStorageEngine {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error>;
}
```

However, RPITIT is **not object-safe**; `dyn AsyncStorageEngine` is rejected by
the compiler.  Because the server must switch backends at runtime (native disk
engine vs. in-memory mock), we need dynamic dispatch.  `async-trait` desugars
`async fn` into a `Box<dyn Future>` return type, making the trait object-safe.

We still use RPITIT in internal non-object-safe helpers where possible, but the
public storage traits use `async-trait`.

### Thread-safety strategy

The native `GraphStorageEngine` uses `&mut self` and is not `Sync`.  The adapter
wraps it in an `Arc<tokio::sync::Mutex<Graph>>` and delegates every call via
`tokio::task::spawn_blocking`.  This guarantees:

* The async runtime never blocks on page I/O or B+ tree traversal.
* CPU-bound work runs on a dedicated OS thread pool (via Tokio's blocking pool).
* The graph state is serialized through the mutex, which is correct until Sprint
  21 introduces a concurrent query execution engine.

### In-memory mock backend

`InMemoryStorageEngine` stores data in a `BTreeMap<Vec<u8>, Vec<u8>>` protected
by a `tokio::sync::RwLock`.  Transactions are implemented as write-ahead
snapshots: a transaction captures the current database snapshot, buffers writes
in a local `BTreeMap`, and applies them atomically on `commit`.  This satisfies
the ACID isolation property "read committed" and is sufficient for property
tests and CI.

## Consequences

* **Positive**: Server code is fully decoupled from the sync storage engine;
  unit tests run in milliseconds without touching disk.
* **Positive**: The trait boundary is documented and versioned, making future
  backend swaps (e.g., NUMA-aware allocator, remote storage) straightforward.
* **Negative**: Every graph operation incurs a `spawn_blocking` + mutex
  overhead.  This is acceptable for Sprint 23 but will be optimised in Sprint 24
  with lock-free structures and per-core shards.
* **Negative**: The in-memory mock does not exercise WAL recovery.  End-to-end
  durability tests still require the native engine.

## References

* `src/server/storage.rs` — trait definitions and implementations
* `src/server/grpc.rs` — consumer of `AsyncGraphEngine`
* Task 11 (rmp) — "Design storage engine async trait (backend-agnostic API)"
