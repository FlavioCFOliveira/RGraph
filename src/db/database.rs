use crate::config::GraphMode;
use crate::graph::builder::{NodeBuilder, RelationshipBuilder};
use crate::graph::engine::{GraphStorageEngine, StorageError};
use crate::graph::graph::{Graph, Node, Relationship};
use crate::graph::record::{SlotRef, ValueType};
use crate::io::FileSystem;
use crate::storage::manager::PageManager;
use crate::wal::writer::WalWriter;
use std::io;
use std::path::{Path, PathBuf};

/// Top-level database handle.
///
/// Owns a fully-initialised [`GraphStorageEngine`] so that CRUD operations
/// are available directly on the handle.  The `page_manager` and `wal_writer`
/// accessors delegate into the inner engine, preserving compatibility with
/// existing tests.
pub struct Database {
    pub path: PathBuf,
    pub graph_mode: GraphMode,
    graph: Graph,
    /// Exclusive inter-process lock held for the lifetime of the handle.
    ///
    /// Declared LAST so it drops after `graph` — the engine's flush-on-Drop runs
    /// while the lock is still held, and the `flock` is released only afterwards.
    /// Holding this real OS lock prevents two processes (or two handles) opening
    /// the same database concurrently and racing the superblock/bitmap/WAL into
    /// incoherence (finding C8).
    _lock: std::fs::File,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .field("graph_mode", &self.graph_mode)
            .finish_non_exhaustive()
    }
}

impl Database {
    pub const LOCK_FILE: &str = ".rgraph.lock";

    // ------------------------------------------------------------------
    // Delegating accessors for backward-compatibility
    // ------------------------------------------------------------------

    /// Borrow the underlying [`PageManager`].
    pub fn page_manager(&self) -> &PageManager {
        &self.graph.engine().page_manager
    }

    /// Mutably borrow the underlying [`PageManager`].
    pub fn page_manager_mut(&mut self) -> &mut PageManager {
        &mut self.graph.engine_mut().page_manager
    }

    /// Borrow the underlying [`WalWriter`].
    pub fn wal_writer(&self) -> &WalWriter {
        &self.graph.engine().wal_writer
    }

    /// Mutably borrow the underlying [`WalWriter`].
    pub fn wal_writer_mut(&mut self) -> &mut WalWriter {
        &mut self.graph.engine_mut().wal_writer
    }

    /// Borrow the inner [`Graph`].
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Mutably borrow the inner [`Graph`].
    pub fn graph_mut(&mut self) -> &mut Graph {
        &mut self.graph
    }

    // ------------------------------------------------------------------
    // High-level CRUD interface
    // ------------------------------------------------------------------

    /// Create a new node.  Returns the `(SlotRef, node_id)` pair.
    pub fn create_node(
        &mut self,
        builder: NodeBuilder,
        fs: &dyn FileSystem,
    ) -> Result<(SlotRef, u64), StorageError> {
        self.graph.create_node(builder, fs)
    }

    /// Retrieve a node by `node_id`.
    pub fn get_node(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<Node>, StorageError> {
        self.graph.get_node(node_id, fs)
    }

    /// Delete a node (tombstone).
    pub fn delete_node(
        &mut self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        self.graph.delete_node(node_id, fs)
    }

    /// Create a new relationship.  Returns the `(SlotRef, edge_id)` pair.
    pub fn create_relationship(
        &mut self,
        builder: RelationshipBuilder,
        fs: &dyn FileSystem,
    ) -> Result<(SlotRef, u64), StorageError> {
        self.graph.create_relationship(builder, fs)
    }

    /// Retrieve a relationship by `edge_id`.
    pub fn get_relationship(
        &self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<Relationship>, StorageError> {
        self.graph.get_relationship(edge_id, fs)
    }

    /// Delete a relationship (tombstone).
    pub fn delete_relationship(
        &mut self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        self.graph.delete_relationship(edge_id, fs)
    }

    /// Scan nodes by label.
    pub fn scan_by_label(
        &self,
        label_id: u32,
        fs: &dyn FileSystem,
    ) -> Result<Vec<Node>, StorageError> {
        self.graph.scan_by_label(label_id, fs)
    }

    /// Scan relationships by type.
    pub fn scan_by_type(
        &self,
        type_id: u32,
        fs: &dyn FileSystem,
    ) -> Result<Vec<Relationship>, StorageError> {
        self.graph.scan_by_type(type_id, fs)
    }

    /// Scan nodes by property value.
    pub fn scan_nodes_by_property(
        &self,
        property_id: u64,
        value_type: ValueType,
        payload: &[u8],
        fs: &dyn FileSystem,
    ) -> Result<Vec<Node>, StorageError> {
        self.graph.scan_nodes_by_property(property_id, value_type, payload, fs)
    }

    /// Flush WAL, superblock, and bitmap to durable storage.
    pub fn sync(&mut self, fs: &dyn FileSystem) -> Result<(), StorageError> {
        self.graph.sync(fs)
    }

    // ------------------------------------------------------------------
    // RDF interface (Task 180)
    // ------------------------------------------------------------------

    /// Is this database operating in RDF mode?
    pub fn is_rdf(&self) -> bool {
        self.graph_mode == GraphMode::Rdf
    }

    /// Insert an RDF triple into the default graph.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexError`] if the database is not in
    /// [`GraphMode::Rdf`], or on a storage failure.  Routing RDF writes only
    /// in RDF mode keeps [`GraphMode::Lpg`] behaviour byte-for-byte unchanged.
    pub fn add_triple(
        &mut self,
        triple: &crate::rdf::Triple,
        fs: &dyn FileSystem,
    ) -> Result<bool, StorageError> {
        if !self.is_rdf() {
            return Err(StorageError::IndexError);
        }
        self.graph.add_triple(triple, fs)
    }

    /// Insert an RDF quad (triple plus optional named graph).  Requires RDF
    /// mode; see [`add_triple`](Database::add_triple).
    pub fn add_quad(
        &mut self,
        quad: &crate::rdf::Quad,
        fs: &dyn FileSystem,
    ) -> Result<bool, StorageError> {
        if !self.is_rdf() {
            return Err(StorageError::IndexError);
        }
        self.graph.add_quad(quad, fs)
    }

    /// Delete an RDF triple from the default graph.  Requires RDF mode.
    pub fn delete_triple(
        &mut self,
        triple: &crate::rdf::Triple,
        fs: &dyn FileSystem,
    ) -> Result<bool, StorageError> {
        if !self.is_rdf() {
            return Err(StorageError::IndexError);
        }
        self.graph.delete_triple(triple, fs)
    }

    /// Match a triple pattern across all graphs (each position may be `None`).
    pub fn match_triples(
        &self,
        subject: Option<&crate::rdf::Term>,
        predicate: Option<&crate::rdf::Term>,
        object: Option<&crate::rdf::Term>,
    ) -> Vec<crate::rdf::Triple> {
        self.graph.match_triples(subject, predicate, object)
    }

    /// Match a quad pattern (each of subject/predicate/object/graph may be
    /// `None`).
    pub fn match_quads(
        &self,
        subject: Option<&crate::rdf::Term>,
        predicate: Option<&crate::rdf::Term>,
        object: Option<&crate::rdf::Term>,
        graph: Option<&crate::rdf::Term>,
    ) -> Vec<crate::rdf::Quad> {
        self.graph.match_quads(subject, predicate, object, graph)
    }

    /// Return every stored RDF quad.
    pub fn all_quads(&self) -> Vec<crate::rdf::Quad> {
        self.graph.all_quads()
    }

    /// Number of distinct RDF triples currently stored.
    pub fn rdf_triple_count(&self) -> usize {
        self.graph.rdf_triple_count()
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    /// Create a new empty database at `path` with the specified `graph_mode`.
    pub fn init(path: &Path, fs: &dyn FileSystem, graph_mode: GraphMode) -> io::Result<Self> {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "database path already exists",
            ));
        }
        fs.create_dir_all(path)?;

        // Acquire a real exclusive inter-process lock (held for the handle's
        // lifetime) before touching any data (finding C8).
        let lock_file = Self::acquire_exclusive_lock(&path.join(Self::LOCK_FILE))?;

        let data_path = path.join(PageManager::DATA_FILE);
        let mut engine = GraphStorageEngine::init(data_path, fs)?;
        // Persist the graph data model in the superblock so a later open under a
        // mismatching mode can be rejected (finding M2).
        engine.page_manager.superblock.graph_mode = graph_mode.to_superblock_code();
        engine.page_manager.sync_superblock(fs)?;
        Ok(Self {
            path: path.to_path_buf(),
            graph_mode,
            graph: Graph::new(engine),
            _lock: lock_file,
        })
    }

    /// Acquire a non-blocking exclusive `flock` on `lock_path` (creating it if
    /// needed).  Returns the held file handle; the lock is released when it
    /// drops.  Fails with `WouldBlock` if another process/handle already holds it.
    fn acquire_exclusive_lock(lock_path: &Path) -> io::Result<std::fs::File> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        // SAFETY: `file` owns a valid fd for the duration of this call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "database is already open by another process (lock held)",
                ));
            }
            return Err(err);
        }
        Ok(file)
    }

    /// Open an existing database, recovering WAL if necessary.
    pub fn open(path: &Path, fs: &dyn FileSystem, graph_mode: GraphMode) -> io::Result<Self> {
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "database path does not exist",
            ));
        }

        // Acquire the real exclusive inter-process lock before opening — a second
        // concurrent open (same process or another) is rejected (finding C8).
        let lock_file = Self::acquire_exclusive_lock(&path.join(Self::LOCK_FILE))?;

        let data_path = path.join(PageManager::DATA_FILE);
        if !fs.exists(&data_path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "database data file missing",
            ));
        }

        let engine = GraphStorageEngine::open(data_path, fs)?;

        // Validate the requested mode against the model the database was created
        // with (finding M2).  The stored mode is authoritative: a database
        // created as RDF must never be reopened as LPG (or vice versa), which
        // would read the same bytes under the wrong data model.  Legacy
        // databases (stored code 0, written before the mode was persisted) carry
        // no model, so the caller's mode is trusted.
        let stored = engine.page_manager.superblock.graph_mode;
        let effective_mode = match GraphMode::from_superblock_code(stored) {
            Some(stored_mode) => {
                if stored_mode != graph_mode {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "database was created in {stored_mode:?} mode but opened as {graph_mode:?}"
                        ),
                    ));
                }
                stored_mode
            }
            None => graph_mode,
        };

        Ok(Self {
            path: path.to_path_buf(),
            graph_mode: effective_mode,
            graph: Graph::new(engine),
            _lock: lock_file,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GraphMode;
    use crate::io::{AlignedBuffer, posix::PosixFileSystem};
    use crate::storage::meta::decode_superblock;
    use crate::storage::page::PAGE_SIZE;

    #[test]
    fn graph_mode_is_persisted_and_validated_on_open() {
        // Regression gate for finding M2 (2026-06-05): the graph data model is
        // persisted in the superblock, survives reopen, and a reopen under a
        // mismatching mode is rejected.
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        // Create as RDF and drop (releasing the lock).
        {
            let _db = Database::init(&db_path, &fs, GraphMode::Rdf).unwrap();
        }

        // Reopening as RDF succeeds and the stored mode is authoritative.
        {
            let db = Database::open(&db_path, &fs, GraphMode::Rdf).unwrap();
            assert_eq!(db.graph_mode, GraphMode::Rdf);
        }

        // Reopening as LPG must be rejected — the bytes belong to an RDF model.
        let err = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn concurrent_open_is_rejected_by_the_lock() {
        // Regression gate for finding C8 (2026-06-05): a second open of the same
        // database while a handle is live must be rejected by the exclusive lock,
        // and succeed again once the first handle is dropped.
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        let db1 = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
        // Second open while db1 holds the lock must fail.
        let err = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        // Release the first handle; a fresh open now succeeds.
        drop(db1);
        let _db2 = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
    }

    #[test]
    fn drop_without_sync_flushes_and_is_recoverable() {
        // Regression gate for finding M21 (2026-06-04): dropping a handle WITHOUT
        // an explicit sync() must still flush (superblock/bitmap/catalog) via the
        // engine's Drop impl, so the node remains discoverable after reopen.
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        let node_id = {
            let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
            let (_slot, id) = db.create_node(NodeBuilder::new().label(7), &fs).unwrap();
            id
            // NOTE: no db.sync(&fs) — the engine's Drop must flush.
        };

        let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
        let node = db
            .get_node(node_id, &fs)
            .unwrap()
            .expect("node must survive a drop without explicit sync");
        assert_eq!(node.node_id, node_id);
        assert_eq!(node.label_id, 7);
    }

    #[test]
    fn init_and_open_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
            assert_eq!(db.page_manager().superblock.total_page_count, 3);
        }

        {
            let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
            assert_eq!(db.page_manager().superblock.total_page_count, 3);
        }
    }

    #[test]
    fn open_rejects_missing_lock() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");
        fs.create_dir_all(&db_path).unwrap();
        let data = db_path.join(PageManager::DATA_FILE);
        fs.open(&data, true).unwrap().sync_data().unwrap();

        let err = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn corrupt_primary_superblock_recover_from_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
            assert_eq!(db.page_manager().superblock.total_page_count, 3);
            drop(db);
        }

        // Corrupt the primary superblock (offset 0).
        let data_path = db_path.join(PageManager::DATA_FILE);
        let handle = fs.open(&data_path, true).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 0).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, 0).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the mirror copy.
        let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
        assert_eq!(db.page_manager().superblock.total_page_count, 3);
    }

    #[test]
    fn corrupt_mirror_superblock_recover_from_primary() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
            drop(db);
        }

        // Corrupt the mirror superblock (offset PAGE_SIZE).
        let data_path = db_path.join(PageManager::DATA_FILE);
        let handle = fs.open(&data_path, true).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, PAGE_SIZE as u64).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, PAGE_SIZE as u64).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the primary copy.
        let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
        assert_eq!(db.page_manager().superblock.total_page_count, 3);
    }

    #[test]
    fn crash_mid_superblock_write_recovers_last_good_generation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
            drop(db);
        }

        let data_path = db_path.join(PageManager::DATA_FILE);
        let handle = fs.open(&data_path, true).unwrap();

        // Read the current mirror superblock to get the last good generation.
        let mut mirror_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut mirror_buf, PAGE_SIZE as u64).unwrap();
        let sb_mirror = decode_superblock(&mirror_buf).unwrap();
        let old_generation = sb_mirror.generation;

        // Simulate a torn write: write a newer generation to primary, then corrupt it.
        let mut primary_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut primary_buf, 0).unwrap();
        let mut sb = decode_superblock(&primary_buf).unwrap();
        sb.generation += 1;
        sb.update_checksum();
        let encoded = crate::storage::meta::encode_superblock(&sb);
        primary_buf[..encoded.len()].copy_from_slice(&encoded);
        primary_buf[30] ^= 0xFF; // corrupt the primary
        handle.write_at(&primary_buf, 0).unwrap();

        // Mirror remains with the old generation.
        handle.write_at(&mirror_buf, PAGE_SIZE as u64).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the mirror with the last good generation.
        let db = Database::open(&db_path, &fs, GraphMode::Lpg).unwrap();
        assert_eq!(db.page_manager().superblock.generation, old_generation);
    }

    #[test]
    fn database_create_and_get_node() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
        let (slot, node_id) = db.create_node(NodeBuilder::new().label(42), &fs).unwrap();
        assert!(!slot.is_null());
        assert!(node_id > 0);

        let node = db.get_node(node_id, &fs).unwrap().unwrap();
        assert_eq!(node.node_id, node_id);
        assert_eq!(node.label_id, 42);
    }

    #[test]
    fn database_graph_mode_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        let db = Database::init(&db_path, &fs, GraphMode::Rdf).unwrap();
        assert_eq!(db.graph_mode, GraphMode::Rdf);
    }
}
