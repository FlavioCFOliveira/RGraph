# Rust API

## Database Initialisation

```rust
use rgraph::db::database::Database;
use rgraph::io::posix::PosixFileSystem;

let fs = PosixFileSystem::new(false);
let db = Database::init("/path/to/db", &fs)?;
```

## Graph Operations

```rust
use rgraph::graph::{
    graph::Graph,
    builder::{NodeBuilder, RelationshipBuilder},
    engine::GraphStorageEngine,
};

let engine = GraphStorageEngine::init(path, &fs)?;
let mut graph = Graph::new(engine);

let slot = graph.create_node(
    NodeBuilder::new(1).label(42).property("name", "Alice"),
    &fs,
)?;
```

## Async Graph Engine

```rust
use rgraph::server::storage::GraphEngineAdapter;

let adapter = GraphEngineAdapter::init(path)?;
let node = adapter.get_node(1).await?;
```

## Error Handling

All public APIs return `Result<T, RGraphError>`.  The error enum covers
I/O, corruption, index, transaction, storage, syntax, semantic, type,
argument, not-found, already-exists, resource-exhausted, and internal
errors.
