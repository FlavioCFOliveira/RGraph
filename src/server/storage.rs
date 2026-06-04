//! Async storage engine traits and implementations.
//!
//! This module defines backend-agnostic async traits for the storage layer,
//! an in-memory mock backend for testing, and an adapter that wraps the
//! synchronous [`GraphStorageEngine`] for use in an async runtime.

use crate::error::RGraphError;
use crate::graph::builder::{NodeBuilder, RelationshipBuilder};
use crate::graph::graph::{Graph, Node, Relationship};
use crate::graph::engine::{GraphStorageEngine, StorageError};
use crate::graph::record::{SlotRef, ValueType};
use crate::io::posix::PosixFileSystem;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

// ------------------------------------------------------------------
// Low-level KV storage traits (Task 11 acceptance criteria)
// ------------------------------------------------------------------

/// Async storage engine trait — backend-agnostic KV interface.
///
/// Exposes `get`, `put`, `delete`, `scan_prefix`, `begin_transaction`,
/// and `snapshot` as required by Sprint 23 / Task 11.
#[async_trait]
pub trait AsyncStorageEngine: Send + Sync {
    /// Retrieve the value associated with `key`.
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RGraphError>;

    /// Store `value` under `key`.
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), RGraphError>;

    /// Remove the entry for `key`.
    async fn delete(&self, key: &[u8]) -> Result<(), RGraphError>;

    /// Scan all keys that start with `prefix`, returning key-value pairs.
    async fn scan_prefix(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, RGraphError>;

    /// Begin a new transaction.
    async fn begin_transaction(&self) -> Result<Box<dyn AsyncTransaction>, RGraphError>;

    /// Capture a read-only snapshot of the current database state.
    async fn snapshot(&self) -> Result<Box<dyn AsyncSnapshot>, RGraphError>;
}

/// An active transaction providing isolated read-write access.
#[async_trait]
pub trait AsyncTransaction: Send + Sync {
    /// Read a value inside the transaction.
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RGraphError>;

    /// Write a value inside the transaction.
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), RGraphError>;

    /// Delete a key inside the transaction.
    async fn delete(&self, key: &[u8]) -> Result<(), RGraphError>;

    /// Commit the transaction.
    async fn commit(&self) -> Result<(), RGraphError>;

    /// Roll back the transaction.
    async fn rollback(&self) -> Result<(), RGraphError>;
}

/// A point-in-time, read-only snapshot.
#[async_trait]
pub trait AsyncSnapshot: Send + Sync {
    /// Read a value visible in the snapshot.
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RGraphError>;

    /// Scan keys with the given prefix visible in the snapshot.
    async fn scan_prefix(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, RGraphError>;
}

// ------------------------------------------------------------------
// High-level async graph engine trait
// ------------------------------------------------------------------

/// Async graph operations backed by any storage implementation.
///
/// This is the trait consumed by the gRPC service layer; it abstracts
/// over concrete backends (native B+ tree, in-memory mock, etc.).
#[async_trait]
pub trait AsyncGraphEngine: Send + Sync {
    /// Create a node and return its slot reference.
    async fn create_node(
        &self,
        builder: NodeBuilder,
    ) -> Result<SlotRef, RGraphError>;

    /// Retrieve a node by ID.
    async fn get_node(
        &self,
        node_id: u64,
    ) -> Result<Option<Node>, RGraphError>;

    /// Delete a node (tombstone).
    async fn delete_node(
        &self,
        node_id: u64,
    ) -> Result<(), RGraphError>;

    /// Create a relationship and return its slot reference.
    async fn create_relationship(
        &self,
        builder: RelationshipBuilder,
    ) -> Result<SlotRef, RGraphError>;

    /// Retrieve a relationship by ID.
    async fn get_relationship(
        &self,
        edge_id: u64,
    ) -> Result<Option<Relationship>, RGraphError>;

    /// Delete a relationship (tombstone).
    async fn delete_relationship(
        &self,
        edge_id: u64,
    ) -> Result<(), RGraphError>;

    /// Scan nodes by label ID.
    async fn scan_nodes_by_label(
        &self,
        label_id: u32,
    ) -> Result<Vec<Node>, RGraphError>;

    /// Scan relationships by type ID.
    async fn scan_relationships_by_type(
        &self,
        type_id: u32,
    ) -> Result<Vec<Relationship>, RGraphError>;

    /// Insert a property index entry.
    async fn insert_property_index(
        &self,
        entity_id: u64,
        property_id: u64,
        value_type: ValueType,
        payload: Vec<u8>,
        slot: SlotRef,
    ) -> Result<(), RGraphError>;

    /// Scan nodes by property value.
    async fn scan_nodes_by_property(
        &self,
        property_id: u64,
        value_type: ValueType,
        payload: Vec<u8>,
    ) -> Result<Vec<Node>, RGraphError>;

    /// Execute a Cypher query through the full Sprint C pipeline
    /// (`parse → semantic analyse → plan → physical execute`).
    ///
    /// `parameters` carries the named parameter bindings (`$name → value`)
    /// that accompany the query.  The returned [`CypherResult`] contains the
    /// ordered column names and the materialised rows.
    ///
    /// # Errors
    ///
    /// Returns a syntax/semantic/type/storage error if any stage of the
    /// pipeline fails.
    async fn execute_cypher(
        &self,
        query: String,
        parameters: std::collections::HashMap<String, crate::cypher::value::Value>,
    ) -> Result<CypherResult, RGraphError>;

    /// Begin a new MVCC transaction and return its server-tracked id.
    ///
    /// The transaction is registered in a server-side map keyed by its
    /// `tx_id`; subsequent [`commit_txn`](AsyncGraphEngine::commit_txn) /
    /// [`rollback_txn`](AsyncGraphEngine::rollback_txn) calls reference it by
    /// that id.  `read_only` selects a snapshot-only isolation level when set.
    async fn begin_txn(&self, read_only: bool) -> Result<u64, RGraphError>;

    /// Commit the transaction previously registered under `tx_id`.
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::NotFound`] if `tx_id` is unknown, and a
    /// [`RGraphError::Transaction`] if the commit is rejected (e.g. an SSI
    /// or write-write conflict) or the WAL flush fails.
    async fn commit_txn(&self, tx_id: u64) -> Result<(), RGraphError>;

    /// Roll back the transaction previously registered under `tx_id`.
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::NotFound`] if `tx_id` is unknown.
    async fn rollback_txn(&self, tx_id: u64) -> Result<(), RGraphError>;

    /// Flush all durable state to disk.
    async fn sync(&self) -> Result<(), RGraphError>;
}

/// Result of executing a Cypher query through [`AsyncGraphEngine::execute_cypher`].
///
/// Mirrors [`crate::cypher::executor::QueryResult`] but lives in the server
/// layer so the gRPC handler does not depend on the executor's row shape.
#[derive(Debug, Clone)]
pub struct CypherResult {
    /// Column names in RETURN order.
    pub columns: Vec<String>,
    /// Result rows; each row aligns positionally with `columns`.
    pub rows: Vec<Vec<crate::cypher::value::Value>>,
}

// ------------------------------------------------------------------
// In-memory mock backend (Task 11 acceptance criteria)
// ------------------------------------------------------------------

/// An in-memory implementation of [`AsyncStorageEngine`] backed by a
/// `BTreeMap<Vec<u8>, Vec<u8>>` protected by a [`RwLock`].
///
/// Suitable for unit tests and CI environments where disk persistence is
/// not required.
pub struct InMemoryStorageEngine {
    data: Arc<RwLock<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl InMemoryStorageEngine {
    /// Create a new empty in-memory engine.
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
}

impl Default for InMemoryStorageEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AsyncStorageEngine for InMemoryStorageEngine {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RGraphError> {
        let data = self.data.read().await;
        Ok(data.get(key).cloned())
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), RGraphError> {
        let mut data = self.data.write().await;
        data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    async fn delete(&self, key: &[u8]) -> Result<(), RGraphError> {
        let mut data = self.data.write().await;
        data.remove(key);
        Ok(())
    }

    async fn scan_prefix(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, RGraphError> {
        let data = self.data.read().await;
        let mut results = Vec::new();
        for (k, v) in data.range(prefix.to_vec()..) {
            if !k.starts_with(prefix) {
                break;
            }
            results.push((k.clone(), v.clone()));
        }
        Ok(results)
    }

    async fn begin_transaction(
        &self) -> Result<Box<dyn AsyncTransaction>, RGraphError> {
        let data = self.data.read().await;
        let snapshot: BTreeMap<Vec<u8>, Vec<u8>> = data.clone();
        drop(data);
        Ok(Box::new(InMemoryTransaction::new(
            self.data.clone(),
            snapshot,
        )))
    }

    async fn snapshot(
        &self) -> Result<Box<dyn AsyncSnapshot>, RGraphError> {
        let data = self.data.read().await;
        let cloned: BTreeMap<Vec<u8>, Vec<u8>> = data.clone();
        Ok(Box::new(InMemorySnapshot { data: cloned }))
    }
}

/// A point-in-time snapshot of the in-memory store.
struct InMemorySnapshot {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
}

#[async_trait]
impl AsyncSnapshot for InMemorySnapshot {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RGraphError> {
        Ok(self.data.get(key).cloned())
    }

    async fn scan_prefix(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, RGraphError> {
        let mut results = Vec::new();
        for (k, v) in self.data.range(prefix.to_vec()..) {
            if !k.starts_with(prefix) {
                break;
            }
            results.push((k.clone(), v.clone()));
        }
        Ok(results)
    }
}

/// An in-memory transaction that buffers writes until commit.
struct InMemoryTransaction {
    store: Arc<RwLock<BTreeMap<Vec<u8>, Vec<u8>>>>,
    snapshot: BTreeMap<Vec<u8>, Vec<u8>>,
    writes: tokio::sync::Mutex<BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
}

impl InMemoryTransaction {
    fn new(
        store: Arc<RwLock<BTreeMap<Vec<u8>, Vec<u8>>>>,
        snapshot: BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Self {
        Self {
            store,
            snapshot,
            writes: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }
}

#[async_trait]
impl AsyncTransaction for InMemoryTransaction {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RGraphError> {
        let writes = self.writes.lock().await;
        if let Some(val) = writes.get(key) {
            return Ok(val.clone());
        }
        drop(writes);
        Ok(self.snapshot.get(key).cloned())
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), RGraphError> {
        let mut writes = self.writes.lock().await;
        writes.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    async fn delete(&self, key: &[u8]) -> Result<(), RGraphError> {
        let mut writes = self.writes.lock().await;
        writes.insert(key.to_vec(), None);
        Ok(())
    }

    async fn commit(&self) -> Result<(), RGraphError> {
        let writes = self.writes.lock().await;
        let write_vec: Vec<(Vec<u8>, Option<Vec<u8>>)> = writes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        drop(writes);
        let mut store = self.store.write().await;
        for (k, v) in write_vec {
            match v {
                Some(val) => {
                    store.insert(k, val);
                }
                None => {
                    store.remove(&k);
                }
            }
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<(), RGraphError> {
        let mut writes = self.writes.lock().await;
        writes.clear();
        Ok(())
    }
}

// ------------------------------------------------------------------
// Adapter: wrap sync GraphStorageEngine for async consumption
// ------------------------------------------------------------------

/// Adapter that exposes the native [`GraphStorageEngine`] through the async
/// [`AsyncGraphEngine`] trait.
///
/// CPU-bound and blocking operations are offloaded to `tokio::task::spawn_blocking`
/// so the async runtime remains responsive.
///
/// # Concurrency
///
/// The inner [`Graph`] is guarded by a [`tokio::sync::RwLock`] (Task 174).
/// Read operations (`get_node`, `get_relationship`, `scan_by_label`,
/// `scan_by_type`, `scan_nodes_by_property`) take a shared read-lock and can
/// run concurrently; write operations take an exclusive write-lock.  This
/// removes the previous global mutex that serialised reads behind writes.
pub struct GraphEngineAdapter {
    inner: Arc<RwLock<Graph>>,
    #[allow(dead_code)]
    fs: PosixFileSystem,
    /// Server-side registry of open MVCC transactions keyed by `tx_id`.
    ///
    /// Entries are inserted by [`begin_txn`](AsyncGraphEngine::begin_txn) and
    /// removed by `commit_txn` / `rollback_txn`.  A [`std::sync::Mutex`] is
    /// used (rather than the Tokio variant) because the guard is only ever
    /// held inside `spawn_blocking` closures, never across an `.await`.
    open_txns: Arc<std::sync::Mutex<std::collections::HashMap<u64, crate::txn::manager::Transaction>>>,
}

impl GraphEngineAdapter {
    /// Wrap an existing [`Graph`] instance.
    pub fn new(graph: Graph) -> Self {
        Self {
            inner: Arc::new(RwLock::new(graph)),
            fs: PosixFileSystem::new(false),
            open_txns: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Return a clone of the shared graph handle.
    ///
    /// Exposed so other server components (e.g. health checks, metrics
    /// collectors) can observe the same engine instance.
    pub fn graph_handle(&self) -> Arc<RwLock<Graph>> {
        self.inner.clone()
    }

    /// Initialise a new graph engine at `path`.
    pub fn init(path: PathBuf) -> Result<Self, RGraphError> {
        let fs = PosixFileSystem::new(false);
        let engine = GraphStorageEngine::init(path, &fs)
            .map_err(|e| RGraphError::Storage(e.to_string()))?;
        let graph = Graph::new(engine);
        Ok(Self::new(graph))
    }
}

#[async_trait]
impl AsyncGraphEngine for GraphEngineAdapter {
    async fn create_node(
        &self,
        builder: NodeBuilder,
    ) -> Result<SlotRef, RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.create_node(builder, &fs)
                .map(|(slot, _id)| slot)
                .map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn get_node(
        &self,
        node_id: u64,
    ) -> Result<Option<Node>, RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard.get_node(node_id, &fs).map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn delete_node(
        &self,
        node_id: u64,
    ) -> Result<(), RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.delete_node(node_id, &fs).map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn create_relationship(
        &self,
        builder: RelationshipBuilder,
    ) -> Result<SlotRef, RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.create_relationship(builder, &fs)
                .map(|(slot, _id)| slot)
                .map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn get_relationship(
        &self,
        edge_id: u64,
    ) -> Result<Option<Relationship>, RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard.get_relationship(edge_id, &fs).map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn delete_relationship(
        &self,
        edge_id: u64,
    ) -> Result<(), RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.delete_relationship(edge_id, &fs).map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn scan_nodes_by_label(
        &self,
        label_id: u32,
    ) -> Result<Vec<Node>, RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard.scan_by_label(label_id, &fs).map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn scan_relationships_by_type(
        &self,
        type_id: u32,
    ) -> Result<Vec<Relationship>, RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard.scan_by_type(type_id, &fs).map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn insert_property_index(
        &self,
        entity_id: u64,
        property_id: u64,
        value_type: ValueType,
        payload: Vec<u8>,
        slot: SlotRef,
    ) -> Result<(), RGraphError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard
                .insert_property_index(entity_id, property_id, value_type, &payload, slot)
                .map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn scan_nodes_by_property(
        &self,
        property_id: u64,
        value_type: ValueType,
        payload: Vec<u8>,
    ) -> Result<Vec<Node>, RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_read();
            guard
                .scan_nodes_by_property(property_id, value_type, &payload, &fs)
                .map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn execute_cypher(
        &self,
        query: String,
        parameters: std::collections::HashMap<String, crate::cypher::value::Value>,
    ) -> Result<CypherResult, RGraphError> {
        use crate::cypher::parser::parse;
        use crate::cypher::physical::{execute_plan, ExecutionContext};
        use crate::cypher::planner::plan;
        use crate::cypher::semantic::analyse;

        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            // Parse → semantic analyse → plan are CPU-only; do them first so we
            // surface syntax/semantic errors before acquiring the write lock.
            let stmt = parse(&query).map_err(|e| RGraphError::Syntax(e.to_string()))?;
            analyse(&stmt).map_err(RGraphError::from)?;
            let logical_plan = plan(&stmt).map_err(RGraphError::from)?;

            // Take the exclusive write lock for the duration of execution so the
            // engine pointer handed to the execution context is unaliased.
            let mut guard = inner.blocking_write();
            let engine = guard.engine_mut();
            let mut ctx = ExecutionContext::new_with_write(engine, &fs);
            ctx.parameters = parameters;

            let result = execute_plan(&logical_plan, &ctx).map_err(RGraphError::from)?;
            Ok(CypherResult {
                columns: result.columns,
                rows: result.rows,
            })
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn begin_txn(&self, read_only: bool) -> Result<u64, RGraphError> {
        use crate::txn::manager::IsolationLevel;

        let inner = self.inner.clone();
        let open_txns = self.open_txns.clone();
        tokio::task::spawn_blocking(move || {
            // Begin against the engine's own transaction manager so the new
            // transaction shares the global MVCC state used by writes.
            let guard = inner.blocking_read();
            let level = if read_only {
                IsolationLevel::RepeatableRead
            } else {
                IsolationLevel::Serializable
            };
            let tx = guard.engine().txn_manager.begin_with_isolation(level);
            let txid = tx.txid;
            drop(guard);

            open_txns
                .lock()
                .map_err(|_| RGraphError::Internal("transaction registry poisoned".into()))?
                .insert(txid, tx);
            Ok(txid)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn commit_txn(&self, tx_id: u64) -> Result<(), RGraphError> {
        let inner = self.inner.clone();
        let open_txns = self.open_txns.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            // Remove the transaction from the registry up front; whatever the
            // outcome, the id is consumed.
            let mut tx = open_txns
                .lock()
                .map_err(|_| RGraphError::Internal("transaction registry poisoned".into()))?
                .remove(&tx_id)
                .ok_or_else(|| RGraphError::NotFound(format!("transaction {tx_id} not found")))?;

            let mut guard = inner.blocking_write();
            let engine = guard.engine_mut();
            // SAFETY of borrow: the txn_manager is cloneable (Arc-backed) so we
            // can take an owned handle and still borrow the WAL mutably.
            let manager = engine.txn_manager.clone();
            manager
                .commit(&mut tx, &mut engine.wal_writer, &fs)
                .map_err(into_tx_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn rollback_txn(&self, tx_id: u64) -> Result<(), RGraphError> {
        let inner = self.inner.clone();
        let open_txns = self.open_txns.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let mut tx = open_txns
                .lock()
                .map_err(|_| RGraphError::Internal("transaction registry poisoned".into()))?
                .remove(&tx_id)
                .ok_or_else(|| RGraphError::NotFound(format!("transaction {tx_id} not found")))?;

            let mut guard = inner.blocking_write();
            let engine = guard.engine_mut();
            let manager = engine.txn_manager.clone();
            manager
                .rollback(&mut tx, &mut engine.wal_writer, &fs)
                .map_err(into_tx_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }

    async fn sync(&self) -> Result<(), RGraphError> {
        let inner = self.inner.clone();
        let fs = PosixFileSystem::new(false);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_write();
            guard.sync(&fs).map_err(into_rgraph_err)
        })
        .await
        .map_err(|e| RGraphError::Internal(e.to_string()))?
    }
}

/// Convert a [`TxError`](crate::txn::manager::TxError) into [`RGraphError`].
fn into_tx_rgraph_err(e: crate::txn::manager::TxError) -> RGraphError {
    use crate::txn::manager::TxError;
    match e {
        TxError::WalFlush(io) => RGraphError::Io(format!("WAL flush failed: {io}")),
        TxError::WoundWait(id) => {
            RGraphError::Transaction(format!("transaction {id} wounded — retry"))
        }
        TxError::NotActive(id, status) => {
            RGraphError::Transaction(format!("transaction {id} not active (status {status:?})"))
        }
        TxError::AlreadyFinalised(id) => {
            RGraphError::Transaction(format!("transaction {id} already finalised"))
        }
        TxError::IndexMutation(msg) => RGraphError::Index(msg),
        TxError::PhantomConflict(id) => {
            RGraphError::Transaction(format!("transaction {id} aborted — phantom/rw-conflict"))
        }
        TxError::WriteConflict(id, res) => RGraphError::Transaction(format!(
            "transaction {id} aborted — write-write conflict on resource {res}"
        )),
    }
}

/// Convert [`StorageError`] into [`RGraphError`].
fn into_rgraph_err(e: StorageError) -> RGraphError {
    match e {
        StorageError::IoError => RGraphError::Io("storage I/O error".into()),
        StorageError::NotFound => RGraphError::NotFound("entity not found".into()),
        StorageError::IndexError => RGraphError::Index("index error".into()),
        StorageError::PageFull => RGraphError::ResourceExhausted("page full".into()),
        StorageError::SlotOverflow => RGraphError::Internal("slot overflow".into()),
        StorageError::AlreadyExists => RGraphError::AlreadyExists("entity already exists".into()),
        StorageError::InvalidId => RGraphError::Argument("id 0 is reserved".into()),
        StorageError::TxConflict => RGraphError::Internal("transaction conflict — retry".into()),
        StorageError::TxAborted => RGraphError::Internal("transaction aborted".into()),
    }
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- InMemoryStorageEngine CRUD property tests ---

    #[tokio::test]
    async fn mem_get_put_roundtrip() {
        let engine = InMemoryStorageEngine::new();
        engine.put(b"hello", b"world").await.unwrap();
        let val = engine.get(b"hello").await.unwrap();
        assert_eq!(val, Some(b"world".to_vec()));
    }

    #[tokio::test]
    async fn mem_delete_removes_key() {
        let engine = InMemoryStorageEngine::new();
        engine.put(b"key", b"val").await.unwrap();
        engine.delete(b"key").await.unwrap();
        let val = engine.get(b"key").await.unwrap();
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn mem_scan_prefix_filters() {
        let engine = InMemoryStorageEngine::new();
        engine.put(b"prefix:a", b"1").await.unwrap();
        engine.put(b"prefix:b", b"2").await.unwrap();
        engine.put(b"other:c", b"3").await.unwrap();

        let results = engine.scan_prefix(b"prefix:").await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn mem_snapshot_isolation() {
        let engine = InMemoryStorageEngine::new();
        engine.put(b"x", b"1").await.unwrap();

        let snap = engine.snapshot().await.unwrap();
        engine.put(b"x", b"2").await.unwrap();

        let old_val = snap.get(b"x").await.unwrap();
        let new_val = engine.get(b"x").await.unwrap();

        assert_eq!(old_val, Some(b"1".to_vec()));
        assert_eq!(new_val, Some(b"2".to_vec()));
    }

    #[tokio::test]
    async fn mem_transaction_commit() {
        let engine = InMemoryStorageEngine::new();
        engine.put(b"a", b"1").await.unwrap();

        let tx = engine.begin_transaction().await.unwrap();
        tx.put(b"a", b"2").await.unwrap();
        tx.put(b"b", b"3").await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(engine.get(b"a").await.unwrap(), Some(b"2".to_vec()));
        assert_eq!(engine.get(b"b").await.unwrap(), Some(b"3".to_vec()));
    }

    #[tokio::test]
    async fn mem_transaction_rollback() {
        let engine = InMemoryStorageEngine::new();
        engine.put(b"a", b"1").await.unwrap();

        let tx = engine.begin_transaction().await.unwrap();
        tx.put(b"a", b"2").await.unwrap();
        tx.rollback().await.unwrap();

        assert_eq!(engine.get(b"a").await.unwrap(), Some(b"1".to_vec()));
    }

    #[tokio::test]
    async fn mem_transaction_delete_and_commit() {
        let engine = InMemoryStorageEngine::new();
        engine.put(b"a", b"1").await.unwrap();

        let tx = engine.begin_transaction().await.unwrap();
        tx.delete(b"a").await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(engine.get(b"a").await.unwrap(), None);
    }

    // --- GraphEngineAdapter tests ---

    #[tokio::test]
    async fn adapter_create_and_get_node() {
        use crate::graph::engine::GraphStorageEngine;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");

        // Create with full API to capture the allocated id.
        let fs = PosixFileSystem::new(false);
        let engine = GraphStorageEngine::init(path, &fs).unwrap();
        let mut graph = crate::graph::graph::Graph::new(engine);
        let (slot, node_id) = graph.create_node(NodeBuilder::new().label(42), &fs).unwrap();
        assert!(!slot.is_null());
        assert!(node_id > 0);

        let node = graph.get_node(node_id, &fs).unwrap();
        assert!(node.is_some());
        assert_eq!(node.unwrap().node_id, node_id);
    }

    #[tokio::test]
    async fn adapter_delete_node() {
        use crate::graph::engine::GraphStorageEngine;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        let engine = GraphStorageEngine::init(path, &fs).unwrap();
        let mut graph = crate::graph::graph::Graph::new(engine);

        let (_, node_id) = graph.create_node(NodeBuilder::new().label(42), &fs).unwrap();
        graph.delete_node(node_id, &fs).unwrap();

        let node = graph.get_node(node_id, &fs).unwrap();
        assert!(node.is_none());
    }

    #[tokio::test]
    async fn adapter_scan_by_label() {
        use crate::graph::engine::GraphStorageEngine;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        let engine = GraphStorageEngine::init(path, &fs).unwrap();
        let mut graph = crate::graph::graph::Graph::new(engine);

        graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();
        graph.create_node(NodeBuilder::new().label(20), &fs).unwrap();
        graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();

        let nodes = graph.scan_by_label(10, &fs).unwrap();
        assert_eq!(nodes.len(), 2);
    }
}
