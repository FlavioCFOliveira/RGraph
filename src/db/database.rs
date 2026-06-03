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

        // Advisory lock to prevent double-open.
        let lock = path.join(Self::LOCK_FILE);
        {
            let handle = fs.open(&lock, true)?;
            handle.sync_data()?;
        }

        let data_path = path.join(PageManager::DATA_FILE);
        let engine = GraphStorageEngine::init(data_path, fs)?;
        Ok(Self {
            path: path.to_path_buf(),
            graph_mode,
            graph: Graph::new(engine),
        })
    }

    /// Open an existing database, recovering WAL if necessary.
    pub fn open(path: &Path, fs: &dyn FileSystem, graph_mode: GraphMode) -> io::Result<Self> {
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "database path does not exist",
            ));
        }

        // Check advisory lock.
        let lock = path.join(Self::LOCK_FILE);
        if !fs.exists(&lock) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "database lock file missing; partial init?",
            ));
        }

        let data_path = path.join(PageManager::DATA_FILE);
        if !fs.exists(&data_path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "database data file missing",
            ));
        }

        let engine = GraphStorageEngine::open(data_path, fs)?;
        Ok(Self {
            path: path.to_path_buf(),
            graph_mode,
            graph: Graph::new(engine),
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
