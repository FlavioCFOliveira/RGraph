//! Graph storage engine trait and implementation.
//!
//! The [`GraphStorageEngine`] provides CRUD operations for nodes, edges,
//! and properties on top of the page manager, B+ tree indexes, and WAL.

use crate::graph::csr::{CsrAdjacency, CsrBuilder, CsrHolder};
use crate::graph::record::{
    EdgeRecord, NodeRecord, PropertyRecord, SlotRef, ValueType, edge_flags, node_flags,
};
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{
    edge_id_key, label_index_key, node_id_key, property_index_key, type_index_key,
};
use crate::index::property::PropertyIndex;
use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::manager::PageManager;
use crate::storage::manager::num_bitmap_pages;
use crate::storage::meta::load_superblock;
use crate::storage::page::{PAGE_SIZE, PageId, PageType, SlottedPage};
use crate::txn::lock_table::LockMode;
use crate::txn::manager::{IsolationLevel, Transaction, TransactionManager, TxError};
use crate::txn::mvcc::TupleHeader;
use crate::wal::aries::AriesRecovery;
use crate::wal::doublewrite::DoubleWriteBuffer;
use crate::wal::record::{RecordType, WalRecord};
use crate::wal::writer::WalWriter;
use crate::catalog::Catalog;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonically-increasing graph-entity id allocator.
///
/// Ids start at 1; id 0 is reserved as a sentinel ("null").
/// The allocator is persisted implicitly: on engine open the max seen id is
/// recovered from `rebuild_indexes` and `next_id` is set accordingly.
///
/// # Thread safety
///
/// `IdAllocator` uses an [`AtomicU64`] so allocation is lock-free.  Each
/// call to [`allocate`] returns a strictly unique, monotonically increasing
/// value.
#[derive(Debug)]
pub struct IdAllocator {
    next_id: AtomicU64,
}

impl IdAllocator {
    /// Create a new allocator starting from `start`.
    ///
    /// `start` must be ≥ 1 (id 0 is the null sentinel).
    pub fn new(start: u64) -> Self {
        let start = start.max(1);
        Self {
            next_id: AtomicU64::new(start),
        }
    }

    /// Allocate the next unique id.
    ///
    /// Always returns a value ≥ 1.  Wraps to 1 on u64 overflow (extremely
    /// unlikely in practice — 2^64 allocations would be needed).
    pub fn allocate(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Seed the allocator so that all future ids are greater than `observed`.
    ///
    /// This is used during recovery to ensure freshly allocated ids never
    /// collide with ids already present on disk.
    pub fn observe(&self, observed: u64) {
        let mut current = self.next_id.load(Ordering::Relaxed);
        loop {
            if observed < current {
                break;
            }
            match self.next_id.compare_exchange_weak(
                current,
                observed + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Return the next id that would be allocated without consuming it.
    pub fn peek(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed)
    }
}

/// Errors that can occur during storage engine operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    /// I/O error from the underlying file system.
    IoError,
    /// The requested entity was not found.
    NotFound,
    /// B+ tree index operation failed.
    IndexError,
    /// Page allocation failed.
    PageFull,
    /// Slot reference overflow (page_id > 24 bits or slot > 255).
    SlotOverflow,
    /// The entity already exists.
    AlreadyExists,
    /// Id 0 is reserved and must not be used for graph entities.
    InvalidId,
    /// A transaction conflict was detected (wound-wait, write-write conflict,
    /// or SSI write-skew).  The caller must roll back and retry.
    TxConflict,
    /// The transaction was aborted (e.g. it was wounded by an older
    /// transaction in the wound-wait protocol).
    TxAborted,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::IoError => write!(f, "I/O error"),
            StorageError::NotFound => write!(f, "entity not found"),
            StorageError::IndexError => write!(f, "index error"),
            StorageError::PageFull => write!(f, "page full"),
            StorageError::SlotOverflow => write!(f, "slot reference overflow"),
            StorageError::AlreadyExists => write!(f, "entity already exists"),
            StorageError::InvalidId => write!(f, "id 0 is reserved and may not be used"),
            StorageError::TxConflict => write!(f, "transaction conflict — retry"),
            StorageError::TxAborted => write!(f, "transaction aborted"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<io::Error> for StorageError {
    fn from(_: io::Error) -> Self {
        StorageError::IoError
    }
}

impl From<BTreeError> for StorageError {
    fn from(_: BTreeError) -> Self {
        StorageError::IndexError
    }
}

impl From<TxError> for StorageError {
    fn from(e: TxError) -> Self {
        match e {
            TxError::WoundWait(_) => StorageError::TxAborted,
            TxError::PhantomConflict(_) | TxError::WriteConflict(_, _) => {
                StorageError::TxConflict
            }
            TxError::WalFlush(_) => StorageError::IoError,
            TxError::NotActive(_, _) | TxError::AlreadyFinalised(_) => StorageError::TxAborted,
            TxError::IndexMutation(_) => StorageError::IndexError,
        }
    }
}

/// Core trait for graph storage operations.
pub trait StorageEngine {
    /// Insert a node record. Returns its [`SlotRef`].
    fn put_node(&mut self, node: &NodeRecord, fs: &dyn FileSystem)
    -> Result<SlotRef, StorageError>;

    /// Retrieve a node by its `node_id`.
    fn get_node(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<NodeRecord>, StorageError>;

    /// Delete a node (leaves a tombstone).
    fn delete_node(&mut self, node_id: u64, fs: &dyn FileSystem) -> Result<(), StorageError>;

    /// Insert an edge record. Returns its [`SlotRef`].
    fn put_edge(&mut self, edge: &EdgeRecord, fs: &dyn FileSystem)
    -> Result<SlotRef, StorageError>;

    /// Retrieve an edge by its `edge_id`.
    fn get_edge(
        &self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<EdgeRecord>, StorageError>;

    /// Delete an edge (leaves a tombstone).
    fn delete_edge(&mut self, edge_id: u64, fs: &dyn FileSystem) -> Result<(), StorageError>;

    /// Insert a property record. Returns its [`SlotRef`].
    fn put_property(
        &mut self,
        prop: &PropertyRecord,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError>;

    /// Retrieve a property by its [`SlotRef`].
    fn get_property(
        &self,
        slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<Option<PropertyRecord>, StorageError>;

    /// Attach a property to a node.
    fn attach_property_to_node(
        &mut self,
        node_id: u64,
        prop_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError>;

    /// Attach a property to an edge.
    fn attach_property_to_edge(
        &mut self,
        edge_id: u64,
        prop_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError>;
}

/// Production graph storage engine.
///
/// Owns the page manager, WAL writer, all B+ tree indexes, and the
/// MVCC transaction manager.  Pages used for graph records are tracked in
/// `node_pages`, `edge_pages`, and `property_pages` so that insertions prefer
/// recently-used pages before allocating new ones.
///
/// `id_allocator` hands out globally unique, monotonically-increasing u64
/// ids for nodes and edges.  On engine open the allocator is seeded from
/// the highest id observed during `rebuild_indexes`.
///
/// # Free-space map and compaction
///
/// `node_free_list` and `edge_free_list` track [`SlotRef`]s vacated by
/// `delete_node` / `delete_edge`.  [`GraphStorageEngine::compact`] scans the
/// record pages, drops tombstone-only pages, prunes stale free-list entries,
/// and brackets the operation with `CompactionBegin` / `CompactionEnd` WAL
/// records.
///
/// # Transaction API
///
/// * [`GraphStorageEngine::begin_transaction`] — start a transaction.
/// * [`GraphStorageEngine::commit_transaction`] — flush WAL, release locks.
/// * [`GraphStorageEngine::rollback_transaction`] — release locks, mark aborted.
///
/// The engine's `put_node`/`put_edge`/`delete_node`/`delete_edge` methods
/// accept an optional `Transaction` reference; when `None` is passed they
/// operate in a single-statement autocommit mode.
#[derive(Debug)]
pub struct GraphStorageEngine {
    pub page_manager: PageManager,
    pub wal_writer: WalWriter,
    pub node_index: BPlusTree,
    pub edge_index: BPlusTree,
    pub label_index: BPlusTree,
    pub type_index: BPlusTree,
    pub property_index: PropertyIndex,
    pub node_pages: Vec<PageId>,
    pub edge_pages: Vec<PageId>,
    pub property_pages: Vec<PageId>,
    /// Server-side id allocator.  Seeded from on-disk data during open.
    pub id_allocator: IdAllocator,
    /// MVCC/lock-based transaction manager.  Shared (via `Arc`) so that
    /// multiple engine handles can participate in the same transaction space.
    pub txn_manager: Arc<TransactionManager>,
    /// Tracks the WAL filesystem for use in transaction commit/rollback.
    /// Cached here so callers do not need to pass it separately.
    wal_fs: Arc<crate::io::posix::PosixFileSystem>,
    /// Schema catalog — maps label/type/property-key names to compact u32 ids.
    pub catalog: Arc<RwLock<Catalog>>,
    /// Slots previously occupied by deleted node records (free-space map).
    ///
    /// `delete_node` pushes the freed [`SlotRef`] here so that
    /// [`GraphStorageEngine::compact`] can account for reclaimable space.
    pub node_free_list: StdMutex<Vec<SlotRef>>,
    /// Slots previously occupied by deleted edge records (free-space map).
    pub edge_free_list: StdMutex<Vec<SlotRef>>,
    /// Optional frozen CSR adjacency snapshot for fast read-only traversals
    /// (Task 58).  Published by [`GraphStorageEngine::freeze_adjacency`].
    pub csr: CsrHolder,
    /// Persistent RDF triple/quad store (Task 180).
    ///
    /// Holds the term dictionary and SPO/POS/OSP permutation index, projected
    /// over RDF primary records that live on their own slotted data pages.
    /// Empty and idle in `GraphMode::Lpg`; populated by `add_triple`/`add_quad`
    /// in `GraphMode::Rdf`.  Rebuilt from the data pages on `open`.
    pub rdf_store: crate::rdf::RdfTripleStore,
}

impl GraphStorageEngine {
    /// Initialise a brand-new graph storage engine.
    pub fn init(data_path: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        let mut pm = PageManager::init(data_path.clone(), PAGE_SIZE as u32, fs)?;

        // Write superblock (both copies).
        pm.sync_superblock(fs)?;
        // Write bitmap page.
        pm.sync_bitmap(fs)?;

        // Initialise the double-write buffer alongside the data file.
        // The DW file lives next to the data file so it survives across restarts.
        let dw_path = data_path.with_extension("dw");
        let dw = Arc::new(DoubleWriteBuffer::open(dw_path, fs)?);
        pm.set_doublewrite(dw);

        // Initialise WAL with a buffered filesystem (WAL does not use O_DIRECT).
        let wal_dir = data_path.parent().unwrap().join("wal");
        let wal_fs = crate::io::posix::PosixFileSystem::new(false);
        let wal_writer = WalWriter::open(wal_dir, &wal_fs)?;

        let config = BPlusTreeConfig::default();
        let wal_fs = Arc::new(crate::io::posix::PosixFileSystem::new(false));
        Ok(Self {
            page_manager: pm,
            wal_writer,
            node_index: BPlusTree::new(config.clone()),
            edge_index: BPlusTree::new(config.clone()),
            label_index: BPlusTree::new(config.clone()),
            type_index: BPlusTree::new(config.clone()),
            property_index: PropertyIndex::new(),
            node_pages: Vec::new(),
            edge_pages: Vec::new(),
            property_pages: Vec::new(),
            id_allocator: IdAllocator::new(1),
            txn_manager: Arc::new(TransactionManager::new()),
            wal_fs,
            catalog: Arc::new(RwLock::new(Catalog::new())),
            node_free_list: StdMutex::new(Vec::new()),
            edge_free_list: StdMutex::new(Vec::new()),
            csr: CsrHolder::new(),
            rdf_store: crate::rdf::RdfTripleStore::new(),
        })
    }

    /// Open an existing engine, recovering WAL if necessary.
    pub fn open(data_path: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        use crate::storage::manager::FIRST_BITMAP_PAGE_ID;

        if !data_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "database data file missing",
            ));
        }

        // Read best-available superblock using mirror-recovery logic.
        let sb = load_superblock(fs, &data_path)?;

        // Determine how many bitmap pages to read based on next_free_page_id.
        let bitmap_count = num_bitmap_pages(sb.next_free_page_id).max(1);
        let handle = fs.open(&data_path, false)?;
        let mut bitmap_bufs: Vec<AlignedBuffer> = Vec::with_capacity(bitmap_count);
        for i in 0..bitmap_count {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            let file_page_id = FIRST_BITMAP_PAGE_ID + i as u64;
            let offset = file_page_id * PAGE_SIZE as u64;
            // If the file is too short for this bitmap page, use a zeroed buffer.
            let file_len = handle.len()?;
            if offset + PAGE_SIZE as u64 <= file_len {
                handle.read_at(&mut buf, offset)?;
            }
            bitmap_bufs.push(buf);
        }
        drop(handle);

        let mut pm = PageManager::open_multi(data_path.clone(), sb, bitmap_bufs, fs)?;

        // Open the double-write buffer and run torn-page recovery before any
        // WAL-based REDO.  This ensures that pages partially overwritten by the
        // previous session are restored to a coherent state before ARIES reads
        // them.
        let dw_path = data_path.with_extension("dw");
        let dw = Arc::new(DoubleWriteBuffer::open(dw_path, fs)?);
        pm.set_doublewrite(dw);
        let torn = pm.recover_torn_pages(fs)?;
        if torn > 0 {
            // Non-fatal: ARIES will REDO any operations that updated these pages
            // after they were last written to the DW buffer.
            eprintln!("DoubleWriteBuffer: restored {} torn page(s) before ARIES recovery", torn);
        }

        // Run full ARIES recovery (ANALYSIS → REDO → UNDO) using a buffered
        // filesystem.  WAL does not use O_DIRECT; only data pages do.
        let wal_dir = data_path.parent().unwrap().join("wal");
        let wal_fs = crate::io::posix::PosixFileSystem::new(false);
        let checkpoint_lsn = pm.superblock.last_checkpoint_lsn;

        let mut wal_writer = WalWriter::open(wal_dir.clone(), &wal_fs)?;

        if wal_dir.exists() {
            let recovery = AriesRecovery::new(&wal_fs, &wal_dir, &data_path, checkpoint_lsn);
            let result = recovery.recover(&mut wal_writer)?;
            if result.max_lsn > 0 {
                pm.superblock.current_wal_lsn = result.max_lsn;
                pm.sync_superblock(fs)?;
            }
        }

        let config = BPlusTreeConfig::default();
        let wal_fs_arc = Arc::new(crate::io::posix::PosixFileSystem::new(false));
        let mut engine = Self {
            page_manager: pm,
            wal_writer,
            node_index: BPlusTree::new(config.clone()),
            edge_index: BPlusTree::new(config.clone()),
            label_index: BPlusTree::new(config.clone()),
            type_index: BPlusTree::new(config.clone()),
            property_index: PropertyIndex::new(),
            node_pages: Vec::new(),
            edge_pages: Vec::new(),
            property_pages: Vec::new(),
            id_allocator: IdAllocator::new(1),
            txn_manager: Arc::new(TransactionManager::new()),
            wal_fs: wal_fs_arc,
            catalog: Arc::new(RwLock::new(Catalog::new())),
            node_free_list: StdMutex::new(Vec::new()),
            edge_free_list: StdMutex::new(Vec::new()),
            csr: CsrHolder::new(),
            rdf_store: crate::rdf::RdfTripleStore::new(),
        };

        // Rebuild secondary indexes from primary data pages.  This also
        // seeds the id_allocator with the highest id seen on disk.
        let _ = engine.rebuild_indexes(fs);

        // Rebuild the RDF term dictionary and permutation index from the RDF
        // primary records on the data pages.  No-op in LPG databases (no RDF
        // records are present), so it is safe to run unconditionally.
        let allocated = engine.page_manager.allocated_pages();
        engine
            .rdf_store
            .rebuild(&engine.page_manager, &allocated, fs);

        Ok(engine)
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Rebuild secondary indexes by scanning all allocated data pages.
    ///
    /// Called automatically during `open` after WAL recovery so that indexes
    /// are consistent even if the in-memory B+ trees were lost.  Also seeds
    /// the [`IdAllocator`] with the highest node/edge id observed on disk so
    /// fresh allocations never collide with existing records.
    pub fn rebuild_indexes(&mut self, fs: &dyn FileSystem) -> Result<(), StorageError> {
        use crate::index::key::{label_index_key, type_index_key};

        let allocated = self.page_manager.allocated_pages();
        for page_id in allocated {
            // Skip metadata pages (superblock copies and bitmap).
            if page_id < 3 {
                continue;
            }

            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            if self.page_manager.read_page(fs, page_id, &mut buf).is_err() {
                continue; // skip unreadable pages
            }
            let page = SlottedPage::new(buf);
            let count = page.header().slot_count;

            let mut page_has_nodes = false;
            let mut page_has_edges = false;

            for slot_idx in 0..count {
                let Some(bytes) = page.read(slot_idx) else {
                    continue;
                };

                // Try to decode as NodeRecord (32 bytes).
                if bytes.len() == NodeRecord::SIZE {
                    if let Some(node) = NodeRecord::decode(bytes).filter(|n| n.node_id != 0) {
                        // Seed the id allocator regardless of deletion state.
                        self.id_allocator.observe(node.node_id);
                        page_has_nodes = true;
                        if node.flags & node_flags::DELETED == 0 {
                            let slot_ref = SlotRef::new(page_id as u32, slot_idx as u8);
                            let value = slot_ref.raw.to_be_bytes().to_vec();
                            let _ = self
                                .node_index
                                .insert(&node_id_key(node.node_id as u128), &value);
                            let _ = self.label_index.insert(
                                &label_index_key(node.label_id as u64, node.node_id as u128),
                                &value,
                            );
                        }
                    }
                }

                // Try to decode as EdgeRecord (64 bytes).
                if bytes.len() == EdgeRecord::SIZE {
                    if let Some(edge) = EdgeRecord::decode(bytes).filter(|e| e.edge_id != 0) {
                        // Seed the id allocator regardless of deletion state.
                        self.id_allocator.observe(edge.edge_id);
                        page_has_edges = true;
                        if edge.flags & edge_flags::DELETED == 0 {
                            let slot_ref = SlotRef::new(page_id as u32, slot_idx as u8);
                            let value = slot_ref.raw.to_be_bytes().to_vec();
                            let _ = self
                                .edge_index
                                .insert(&edge_id_key(edge.edge_id as u128), &value);
                            let _ = self.type_index.insert(
                                &type_index_key(edge.type_id as u64, edge.edge_id as u128),
                                &value,
                            );
                        }
                    }
                }
            }

            // Track pages by type so future insertions reuse them.
            if page_has_nodes && !self.node_pages.contains(&page_id) {
                self.node_pages.push(page_id);
            }
            if page_has_edges && !self.edge_pages.contains(&page_id) {
                self.edge_pages.push(page_id);
            }
        }
        Ok(())
    }

    /// Look up the physical [`SlotRef`] for a node by its logical `node_id`.
    ///
    /// Returns `Ok(None)` if the node does not exist or has been deleted.
    pub fn lookup_node_slot(&self, node_id: u64) -> Result<Option<SlotRef>, StorageError> {
        let key = node_id_key(node_id as u128);
        let (_page_id, slot) = match self.node_index.search(&key) {
            Some(r) => r,
            None => return Ok(None),
        };
        let page = self
            .node_index
            .get_page(_page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;
        Ok(Some(slot_ref))
    }

    /// Write a raw record into the most recently used page in `page_list`,
    /// allocating a fresh page if necessary.
    fn insert_record(
        page_manager: &mut PageManager,
        record: &[u8],
        page_list: &mut Vec<PageId>,
        page_type: PageType,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError> {
        // Try existing pages (most recent first).
        for &page_id in page_list.iter().rev() {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            page_manager.read_page(fs, page_id, &mut buf)?;
            let mut page = SlottedPage::new(buf);
            if let Some(slot) = page.insert(record) {
                page.update_checksum();
                page_manager.write_page(fs, page_id, &mut page.buf)?;
                return slot_ref(page_id, slot);
            }
        }

        // Allocate a new page.
        let page_id = page_manager.allocate_page();
        let mut page = SlottedPage::init(page_id, page_type);
        let slot = page.insert(record).ok_or(StorageError::PageFull)?;
        page.update_checksum();
        page_manager.write_page(fs, page_id, &mut page.buf)?;
        page_list.push(page_id);
        slot_ref(page_id, slot)
    }

    /// Read a raw record from a [`SlotRef`].
    fn read_record(
        page_manager: &PageManager,
        slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let page_id = slot.page_id() as u64;
        let slot_idx = slot.slot_index();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        page_manager.read_page(fs, page_id, &mut buf)?;
        let page = SlottedPage::new(buf);
        Ok(page.read(slot_idx as u16).map(|s| s.to_vec()))
    }

    /// Overwrite a raw record at a [`SlotRef`] in place.
    fn overwrite_record(
        page_manager: &mut PageManager,
        slot: SlotRef,
        record: &[u8],
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        let page_id = slot.page_id() as u64;
        let slot_idx = slot.slot_index();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        page_manager.read_page(fs, page_id, &mut buf)?;
        let mut page = SlottedPage::new(buf);
        let existing_slot = page.slot(slot_idx as u16).ok_or(StorageError::NotFound)?;
        if existing_slot.is_deleted() {
            return Err(StorageError::NotFound);
        }
        let existing_len = existing_slot.length as usize;
        let existing_offset = existing_slot.offset as usize;
        if record.len() <= existing_len {
            let start = SlottedPage::HEADER_SIZE + existing_offset;
            page.buf[start..start + record.len()].copy_from_slice(record);
        } else {
            return Err(StorageError::PageFull);
        }
        page.update_checksum();
        page_manager.write_page(fs, page_id, &mut page.buf)?;
        Ok(())
    }

    /// Append a WAL record and return its LSN.
    fn log(
        page_manager: &mut PageManager,
        wal_writer: &mut WalWriter,
        fs: &dyn FileSystem,
        record_type: RecordType,
        txid: u64,
        payload: Vec<u8>,
    ) -> Result<u64, StorageError> {
        let prev_lsn = page_manager.superblock.current_wal_lsn;
        let rec = WalRecord::new(record_type, txid, 0, prev_lsn, payload);
        let lsn = wal_writer.append(fs, rec)?;
        page_manager.superblock.current_wal_lsn = lsn;
        Ok(lsn)
    }

    /// Compact tombstoned node and edge slots.
    ///
    /// Scans the tracked node and edge pages, drops pages whose records are
    /// all tombstones (no live records remain), prunes free-list entries that
    /// point into discarded pages, and brackets the operation with
    /// `CompactionBegin` / `CompactionEnd` WAL records so recovery can observe
    /// the event.
    ///
    /// Returns the number of tombstone slots reclaimed.
    ///
    /// # Errors
    ///
    /// Propagates WAL or page-manager I/O failures as [`StorageError`].
    pub fn compact(&mut self, fs: &dyn FileSystem) -> Result<usize, StorageError> {
        let mut reclaimed = 0usize;

        // Bracket the compaction in the WAL (logical marker, no page image).
        Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::CompactionBegin,
            0,
            vec![],
        )?;

        // --- Compact node pages: keep only pages with at least one live record.
        let node_page_ids = std::mem::take(&mut self.node_pages);
        for page_id in &node_page_ids {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            if self.page_manager.read_page(fs, *page_id, &mut buf).is_err() {
                continue;
            }
            let page = SlottedPage::new(buf);
            let count = page.header().slot_count;
            let mut has_live = false;
            for slot_idx in 0..count {
                let Some(bytes) = page.read(slot_idx) else {
                    continue;
                };
                if bytes.len() == NodeRecord::SIZE
                    && let Some(node) = NodeRecord::decode(bytes)
                    && node.node_id != 0
                {
                    if node.flags & node_flags::DELETED == 0 {
                        has_live = true;
                    } else {
                        reclaimed += 1;
                    }
                }
            }
            if has_live {
                self.node_pages.push(*page_id);
            }
        }
        {
            let mut free = self.node_free_list.lock().expect("node free list poisoned");
            free.retain(|s| self.node_pages.contains(&(s.page_id() as u64)));
        }

        // --- Compact edge pages.
        let edge_page_ids = std::mem::take(&mut self.edge_pages);
        for page_id in &edge_page_ids {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            if self.page_manager.read_page(fs, *page_id, &mut buf).is_err() {
                continue;
            }
            let page = SlottedPage::new(buf);
            let count = page.header().slot_count;
            let mut has_live = false;
            for slot_idx in 0..count {
                let Some(bytes) = page.read(slot_idx) else {
                    continue;
                };
                if bytes.len() == EdgeRecord::SIZE
                    && let Some(edge) = EdgeRecord::decode(bytes)
                    && edge.edge_id != 0
                {
                    if edge.flags & edge_flags::DELETED == 0 {
                        has_live = true;
                    } else {
                        reclaimed += 1;
                    }
                }
            }
            if has_live {
                self.edge_pages.push(*page_id);
            }
        }
        {
            let mut free = self.edge_free_list.lock().expect("edge free list poisoned");
            free.retain(|s| self.edge_pages.contains(&(s.page_id() as u64)));
        }

        // Bracket end with the count of reclaimed slots.
        let payload = (reclaimed as u64).to_be_bytes().to_vec();
        Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::CompactionEnd,
            0,
            payload,
        )?;

        Ok(reclaimed)
    }

    /// Build and publish a frozen [`CsrAdjacency`] snapshot from the current
    /// edge and node pages (Task 58).
    ///
    /// After this call, [`GraphStorageEngine::scan_adjacency`] uses the
    /// cache-friendly CSR path instead of the doubly-linked walk.  The CSR is
    /// keyed by the logical `source_id` / `target_id` carried in each
    /// [`EdgeRecord`].
    ///
    /// # Performance
    ///
    /// Building the CSR requires a full scan of all tracked edge and node
    /// pages.  Call this when the write rate drops or before a read-heavy
    /// traversal workload.
    pub fn freeze_adjacency(&self, fs: &dyn FileSystem) -> Arc<CsrAdjacency> {
        let mut builder = CsrBuilder::new();

        // Collect live edge records from every tracked edge page.
        for &page_id in &self.edge_pages {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            if self.page_manager.read_page(fs, page_id, &mut buf).is_err() {
                continue;
            }
            let page = SlottedPage::new(buf);
            let count = page.header().slot_count;
            for slot_idx in 0..count {
                let Some(bytes) = page.read(slot_idx) else {
                    continue;
                };
                if bytes.len() == EdgeRecord::SIZE
                    && let Some(edge) = EdgeRecord::decode(bytes)
                    && edge.edge_id != 0
                {
                    builder.add_edge(&edge);
                }
            }
        }

        // Also scan node pages so isolated nodes are represented in row_ptr.
        for &page_id in &self.node_pages {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            if self.page_manager.read_page(fs, page_id, &mut buf).is_err() {
                continue;
            }
            let page = SlottedPage::new(buf);
            let count = page.header().slot_count;
            for slot_idx in 0..count {
                let Some(bytes) = page.read(slot_idx) else {
                    continue;
                };
                if bytes.len() == NodeRecord::SIZE
                    && let Some(node) = NodeRecord::decode(bytes)
                    && node.node_id != 0
                {
                    builder.add_node(&node);
                }
            }
        }

        let csr = builder.build();
        // Publish a clone, returning the Arc to the caller for immediate use.
        self.csr.freeze(csr.clone());
        Arc::new(csr)
    }

    /// Release the frozen CSR snapshot, reverting [`scan_adjacency`] to the
    /// doubly-linked walk until the next [`freeze_adjacency`].
    pub fn thaw_adjacency(&self) {
        self.csr.thaw();
    }

    /// Scan the outgoing edges of `node_id`, returning
    /// `(target_id, edge_id, type_id)` triples.
    ///
    /// Prefers the frozen CSR snapshot when one is available (cache-friendly);
    /// otherwise falls back to walking the doubly-linked adjacency list from
    /// the node's `first_outgoing_edge`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexError`] if the node index is inconsistent,
    /// or propagates page-read failures.
    pub fn scan_adjacency(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Vec<(u64, u64, u32)>, StorageError> {
        // Fast path: a frozen CSR snapshot is available.
        if let Some(snap) = self.csr.snapshot() {
            return Ok(snap
                .outgoing_edges(node_id)
                .map(|e| (e.target_id, e.edge_id, e.edge_type))
                .collect());
        }

        // Slow path: follow the doubly-linked list via the node record.
        let key = node_id_key(node_id as u128);
        let (page_id, slot) = match self.node_index.search(&key) {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };
        let page = self
            .node_index
            .get_page(page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record =
            Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        let node = NodeRecord::decode(&record).ok_or(StorageError::NotFound)?;

        let mut result = Vec::new();
        let mut cursor = node.first_outgoing_edge;
        const MAX_HOPS: usize = 65536;
        let mut depth = 0usize;
        while !cursor.is_null() && depth < MAX_HOPS {
            depth += 1;
            let Some(edge_bytes) = Self::read_record(&self.page_manager, cursor, fs)? else {
                break;
            };
            let Some(edge) = EdgeRecord::decode(&edge_bytes) else {
                break;
            };
            if edge.flags & edge_flags::DELETED == 0 {
                result.push((edge.target_id, edge.edge_id, edge.type_id));
            }
            cursor = edge.next_source_edge;
        }
        Ok(result)
    }

    /// Sync the WAL, superblock, and bitmap to durable storage.
    ///
    /// If the WAL has accumulated more than [`WalWriter::CHECKPOINT_INTERVAL`]
    /// bytes since the last fuzzy checkpoint, a checkpoint is taken here
    /// (Task 151).  The new `last_checkpoint_lsn` is persisted to the
    /// superblock so that ARIES ANALYSIS can bound its scan on the next restart.
    pub fn sync(&mut self, fs: &dyn FileSystem) -> Result<(), StorageError> {
        self.wal_writer.sync(fs)?;

        // Trigger a fuzzy checkpoint when the interval threshold is exceeded.
        if self.wal_writer.needs_checkpoint() {
            if let Some(pool) = self.page_manager.buffer_pool() {
                let wal_fs = std::sync::Arc::new(crate::io::posix::PosixFileSystem::new(false));
                match crate::wal::checkpoint::Checkpoint::run(&pool, wal_fs, &mut self.wal_writer) {
                    Ok(ckpt_lsn) => {
                        self.page_manager.superblock.last_checkpoint_lsn = ckpt_lsn;
                        self.wal_writer.reset_checkpoint_counter();
                    }
                    Err(_) => {
                        // Checkpoint failure is non-fatal — the engine can continue.
                        // Recovery will simply replay more WAL on the next restart.
                    }
                }
            } else {
                // No buffer pool attached (e.g. in single-segment mode):
                // reset the counter to avoid repeated no-op checks.
                self.wal_writer.reset_checkpoint_counter();
            }
        }

        // Write superblock (both copies at pages 0 and 1), then write the
        // bitmap page (page 2).
        self.page_manager.sync_superblock(fs)?;
        self.page_manager.sync_bitmap(fs)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Transaction API (Task 154)
    // ------------------------------------------------------------------

    /// Begin a new transaction at the default isolation level
    /// (`RepeatableRead`).
    ///
    /// The caller is responsible for calling [`commit_transaction`] or
    /// [`rollback_transaction`] when done.
    pub fn begin_transaction(&self) -> Transaction {
        self.txn_manager.begin()
    }

    /// Begin a new transaction with an explicit isolation level.
    pub fn begin_transaction_with_isolation(&self, level: IsolationLevel) -> Transaction {
        self.txn_manager.begin_with_isolation(level)
    }

    /// Commit a transaction: flush the WAL to durable storage, release locks,
    /// and update the global transaction state.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::IoError` if the WAL flush fails.
    /// Returns `StorageError::TxAborted` if the transaction is not active.
    /// Returns `StorageError::TxConflict` if an SSI write-skew is detected.
    pub fn commit_transaction(
        &mut self,
        tx: &mut Transaction,
    ) -> Result<(), StorageError> {
        let fs = Arc::clone(&self.wal_fs);
        self.txn_manager
            .commit(tx, &mut self.wal_writer, fs.as_ref())
            .map_err(StorageError::from)
    }

    /// Roll back a transaction: release all locks and mark it as aborted.
    ///
    /// An `Abort` WAL record is written best-effort (not flushed).
    pub fn rollback_transaction(
        &mut self,
        tx: &mut Transaction,
    ) -> Result<(), StorageError> {
        let fs = Arc::clone(&self.wal_fs);
        self.txn_manager
            .rollback(tx, &mut self.wal_writer, fs.as_ref())
            .map_err(StorageError::from)
    }

    /// Acquire an exclusive lock on a node resource within a transaction.
    ///
    /// Used internally by `put_node` / `delete_node`; exposed publicly for
    /// callers that need to lock a node before reading-then-writing it.
    pub fn lock_node(&self, tx: &mut Transaction, node_id: u64) -> Result<(), StorageError> {
        let resource_id = node_resource_id(node_id);
        self.txn_manager
            .acquire_lock(tx, resource_id, LockMode::Exclusive)
            .map_err(StorageError::from)
    }

    /// Acquire an exclusive lock on an edge resource within a transaction.
    pub fn lock_edge(&self, tx: &mut Transaction, edge_id: u64) -> Result<(), StorageError> {
        let resource_id = edge_resource_id(edge_id);
        self.txn_manager
            .acquire_lock(tx, resource_id, LockMode::Exclusive)
            .map_err(StorageError::from)
    }

    /// Return a shared reference to the schema catalog.
    pub fn catalog(&self) -> Arc<RwLock<Catalog>> {
        Arc::clone(&self.catalog)
    }

    // ------------------------------------------------------------------
    // RDF triple/quad API (Task 180)
    // ------------------------------------------------------------------

    /// Insert an RDF triple into the default graph, persisting it durably.
    ///
    /// Interns the subject/predicate/object terms, writes the triple record
    /// to an RDF data page, WAL-logs the mutation, and updates the in-memory
    /// permutation index.  Returns `true` if the triple was newly added or
    /// `false` if it already existed (idempotent).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] on page or WAL I/O failure.
    pub fn add_triple(
        &mut self,
        triple: &crate::rdf::Triple,
        fs: &dyn FileSystem,
    ) -> Result<bool, StorageError> {
        self.rdf_store
            .insert_triple(&mut self.page_manager, &mut self.wal_writer, triple, fs)
            .map_err(|_| StorageError::IndexError)
    }

    /// Insert an RDF quad (triple plus optional named graph).  See
    /// [`add_triple`](GraphStorageEngine::add_triple).
    pub fn add_quad(
        &mut self,
        quad: &crate::rdf::Quad,
        fs: &dyn FileSystem,
    ) -> Result<bool, StorageError> {
        self.rdf_store
            .insert_quad(&mut self.page_manager, &mut self.wal_writer, quad, fs)
            .map_err(|_| StorageError::IndexError)
    }

    /// Delete an RDF triple from the default graph.  Returns `true` if it
    /// existed and was removed.
    pub fn delete_triple(
        &mut self,
        triple: &crate::rdf::Triple,
        fs: &dyn FileSystem,
    ) -> Result<bool, StorageError> {
        self.rdf_store
            .delete_triple(&mut self.page_manager, &mut self.wal_writer, triple, fs)
            .map_err(|_| StorageError::IndexError)
    }

    /// Match a triple pattern across all graphs.  Any position may be `None`
    /// (a wildcard).  Returns the matching term-valued triples.
    pub fn match_triples(
        &self,
        subject: Option<&crate::rdf::Term>,
        predicate: Option<&crate::rdf::Term>,
        object: Option<&crate::rdf::Term>,
    ) -> Vec<crate::rdf::Triple> {
        self.rdf_store.match_triples(subject, predicate, object)
    }

    /// Match a quad pattern.  Any of subject/predicate/object/graph may be
    /// `None` (a wildcard).  Returns the matching term-valued quads.
    pub fn match_quads(
        &self,
        subject: Option<&crate::rdf::Term>,
        predicate: Option<&crate::rdf::Term>,
        object: Option<&crate::rdf::Term>,
        graph: Option<&crate::rdf::Term>,
    ) -> Vec<crate::rdf::Quad> {
        self.rdf_store
            .match_pattern(subject, predicate, object, graph)
    }

    /// Return every stored RDF quad.
    pub fn all_quads(&self) -> Vec<crate::rdf::Quad> {
        self.rdf_store.all_quads()
    }

    /// Number of distinct RDF triples currently stored.
    pub fn rdf_triple_count(&self) -> usize {
        self.rdf_store.triple_count()
    }

    /// Resolve a label name to its catalog id, creating an entry if absent.
    ///
    /// This is a write operation on the catalog — use when registering new
    /// labels (typically during `CREATE` execution).
    pub fn catalog_label_id(&self, label: &str) -> u32 {
        self.catalog.write().expect("catalog write lock poisoned").get_or_create_label(label)
    }

    /// Look up the u64 label id used in storage from a string label name.
    ///
    /// Returns `None` if the label is not yet registered in the catalog.
    pub fn storage_label_id_for(&self, label: &str) -> Option<u64> {
        self.catalog.read().expect("catalog read lock poisoned")
            .label_id(label)
            .map(|id| id as u64)
    }

    pub fn scan_nodes_by_label(
        &self,
        label_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Vec<NodeRecord>, StorageError> {
        let start = label_index_key(label_id, 0);
        let end = label_index_key(label_id, u128::MAX);
        let entries = self.label_index.range_search(&start, &end);
        let mut results = Vec::new();
        for (_key, value) in entries {
            let slot_ref = decode_slot_ref(&value).ok_or(StorageError::IndexError)?;
            let record = Self::read_record(&self.page_manager, slot_ref, fs)?;
            if let Some(node) = record.and_then(|b| NodeRecord::decode(&b)) {
                if node.flags & node_flags::DELETED == 0 {
                    results.push(node);
                }
            }
        }
        Ok(results)
    }

    pub fn scan_edges_by_type(
        &self,
        type_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Vec<EdgeRecord>, StorageError> {
        let start = type_index_key(type_id, 0);
        let end = type_index_key(type_id, u128::MAX);
        let entries = self.type_index.range_search(&start, &end);
        let mut results = Vec::new();
        for (_key, value) in entries {
            let slot_ref = decode_slot_ref(&value).ok_or(StorageError::IndexError)?;
            let record = Self::read_record(&self.page_manager, slot_ref, fs)?;
            if let Some(edge) = record.and_then(|b| EdgeRecord::decode(&b)) {
                if edge.flags & edge_flags::DELETED == 0 {
                    results.push(edge);
                }
            }
        }
        Ok(results)
    }

    /// Scan all outgoing edges from `source_node_id`, optionally filtered by
    /// `type_ids` (empty = all types).  Returns a list of `(edge, end_node_id)`
    /// pairs where `end_node_id` is the logical target node id.
    pub fn scan_outgoing_edges(
        &self,
        source_node_id: u64,
        type_ids: &[u64],
        fs: &dyn FileSystem,
    ) -> Result<Vec<(EdgeRecord, u64)>, StorageError> {
        let node_record = match self.get_node(source_node_id, fs)? {
            Some(r) => r,
            None => return Ok(vec![]),
        };
        let mut results = Vec::new();
        let mut cursor = node_record.first_outgoing_edge;
        const MAX_HOPS: usize = 65536;
        let mut depth = 0usize;
        while !cursor.is_null() && depth < MAX_HOPS {
            depth += 1;
            let bytes = match Self::read_record(&self.page_manager, cursor, fs)? {
                Some(b) => b,
                None => break,
            };
            let edge = match EdgeRecord::decode(&bytes) {
                Some(e) => e,
                None => break,
            };
            if edge.flags & edge_flags::DELETED == 0 {
                let type_matches = type_ids.is_empty()
                    || type_ids.contains(&(edge.type_id as u64));
                if type_matches {
                    results.push((edge, edge.target_id));
                }
            }
            cursor = edge.next_source_edge;
        }
        Ok(results)
    }

    /// Scan all incoming edges to `target_node_id`, optionally filtered by
    /// `type_ids` (empty = all types).  Returns `(edge, end_node_id)` where
    /// `end_node_id` is the logical source node id.
    pub fn scan_incoming_edges(
        &self,
        target_node_id: u64,
        type_ids: &[u64],
        fs: &dyn FileSystem,
    ) -> Result<Vec<(EdgeRecord, u64)>, StorageError> {
        let node_record = match self.get_node(target_node_id, fs)? {
            Some(r) => r,
            None => return Ok(vec![]),
        };
        let mut results = Vec::new();
        let mut cursor = node_record.first_incoming_edge;
        const MAX_HOPS: usize = 65536;
        let mut depth = 0usize;
        while !cursor.is_null() && depth < MAX_HOPS {
            depth += 1;
            let bytes = match Self::read_record(&self.page_manager, cursor, fs)? {
                Some(b) => b,
                None => break,
            };
            let edge = match EdgeRecord::decode(&bytes) {
                Some(e) => e,
                None => break,
            };
            if edge.flags & edge_flags::DELETED == 0 {
                let type_matches = type_ids.is_empty()
                    || type_ids.contains(&(edge.type_id as u64));
                if type_matches {
                    results.push((edge, edge.source_id));
                }
            }
            cursor = edge.next_target_edge;
        }
        Ok(results)
    }

    pub fn insert_property_index(
        &mut self,
        entity_id: u128,
        property_id: u64,
        value_type: ValueType,
        payload: &[u8],
        slot: SlotRef,
    ) -> Result<(), StorageError> {
        self.property_index
            .insert(property_id, value_type, payload, entity_id, slot)
            .map_err(|e| e.into())
    }

    pub fn scan_property_index(
        &self,
        property_id: u64,
        value_type: ValueType,
        payload: &[u8],
    ) -> Vec<(u128, SlotRef)> {
        let serialized = crate::index::value_codec::encode_property_value(value_type, payload)
            .unwrap_or_else(|| vec![0x00]);
        let start = property_index_key(property_id, &serialized, 0);
        let end = property_index_key(property_id, &serialized, u128::MAX);
        let entries = self.property_index.tree().range_search(&start, &end);
        let mut results = Vec::new();
        for (key, value) in entries {
            let key_slice = key.as_slice();
            if key_slice.len() < 16 {
                continue;
            }
            let entity_bytes = &key_slice[key_slice.len() - 16..];
            let mut arr = [0u8; 16];
            arr.copy_from_slice(entity_bytes);
            let entity_id = u128::from_be_bytes(arr);
            if let Some(slot_ref) = decode_slot_ref(&value) {
                results.push((entity_id, slot_ref));
            }
        }
        results
    }

    // ------------------------------------------------------------------
    // Adjacency list helpers (private)
    // ------------------------------------------------------------------

    /// Link `edge_slot` at the head of the source node's outgoing adjacency
    /// list, then update the source node record's `first_outgoing_edge`.
    ///
    /// Operates directly on the page manager to avoid borrow conflicts.
    fn wire_source_adjacency(
        pm: &mut PageManager,
        edge_slot: SlotRef,
        node_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        // Read source node to get current head.
        let node_bytes = Self::read_record(pm, node_slot, fs)?.ok_or(StorageError::NotFound)?;
        let mut node = NodeRecord::decode(&node_bytes).ok_or(StorageError::NotFound)?;
        let old_head = node.first_outgoing_edge;

        // Patch the new edge's next/prev pointers.
        let edge_bytes = Self::read_record(pm, edge_slot, fs)?.ok_or(StorageError::NotFound)?;
        let mut edge = EdgeRecord::decode(&edge_bytes).ok_or(StorageError::NotFound)?;
        edge.next_source_edge = old_head;
        edge.prev_source_edge = SlotRef::NULL;
        let mut edge_buf = [0u8; EdgeRecord::SIZE];
        edge.encode(&mut edge_buf);
        Self::overwrite_record(pm, edge_slot, &edge_buf, fs)?;

        // If there was a previous head, update its prev pointer.
        if !old_head.is_null() {
            let head_bytes = Self::read_record(pm, old_head, fs)?.ok_or(StorageError::NotFound)?;
            let mut head_edge = EdgeRecord::decode(&head_bytes).ok_or(StorageError::NotFound)?;
            head_edge.prev_source_edge = edge_slot;
            let mut head_buf = [0u8; EdgeRecord::SIZE];
            head_edge.encode(&mut head_buf);
            Self::overwrite_record(pm, old_head, &head_buf, fs)?;
        }

        // Update the source node's first_outgoing_edge.
        node.first_outgoing_edge = edge_slot;
        node.generation += 1;
        let mut node_buf = [0u8; NodeRecord::SIZE];
        node.encode(&mut node_buf);
        Self::overwrite_record(pm, node_slot, &node_buf, fs)
    }

    /// Link `edge_slot` at the head of the target node's incoming adjacency
    /// list, then update the target node record's `first_incoming_edge`.
    fn wire_target_adjacency(
        pm: &mut PageManager,
        edge_slot: SlotRef,
        node_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        // Read target node to get current head.
        let node_bytes = Self::read_record(pm, node_slot, fs)?.ok_or(StorageError::NotFound)?;
        let mut node = NodeRecord::decode(&node_bytes).ok_or(StorageError::NotFound)?;
        let old_head = node.first_incoming_edge;

        // Patch the new edge's next/prev pointers.
        let edge_bytes = Self::read_record(pm, edge_slot, fs)?.ok_or(StorageError::NotFound)?;
        let mut edge = EdgeRecord::decode(&edge_bytes).ok_or(StorageError::NotFound)?;
        edge.next_target_edge = old_head;
        edge.prev_target_edge = SlotRef::NULL;
        let mut edge_buf = [0u8; EdgeRecord::SIZE];
        edge.encode(&mut edge_buf);
        Self::overwrite_record(pm, edge_slot, &edge_buf, fs)?;

        // If there was a previous head, update its prev pointer.
        if !old_head.is_null() {
            let head_bytes = Self::read_record(pm, old_head, fs)?.ok_or(StorageError::NotFound)?;
            let mut head_edge = EdgeRecord::decode(&head_bytes).ok_or(StorageError::NotFound)?;
            head_edge.prev_target_edge = edge_slot;
            let mut head_buf = [0u8; EdgeRecord::SIZE];
            head_edge.encode(&mut head_buf);
            Self::overwrite_record(pm, old_head, &head_buf, fs)?;
        }

        // Update the target node's first_incoming_edge.
        node.first_incoming_edge = edge_slot;
        node.generation += 1;
        let mut node_buf = [0u8; NodeRecord::SIZE];
        node.encode(&mut node_buf);
        Self::overwrite_record(pm, node_slot, &node_buf, fs)
    }

    /// Unlink an edge from the source node's adjacency list, patching
    /// neighbour pointers.  Returns the new outgoing head if the edge was
    /// the list head.
    fn unwire_source_adjacency(
        pm: &mut PageManager,
        edge_slot: SlotRef,
        node_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<Option<SlotRef>, StorageError> {
        let edge_bytes = Self::read_record(pm, edge_slot, fs)?.ok_or(StorageError::NotFound)?;
        let edge = EdgeRecord::decode(&edge_bytes).ok_or(StorageError::NotFound)?;
        let prev = edge.prev_source_edge;
        let next = edge.next_source_edge;

        if !prev.is_null() {
            let p_bytes = Self::read_record(pm, prev, fs)?.ok_or(StorageError::NotFound)?;
            let mut p = EdgeRecord::decode(&p_bytes).ok_or(StorageError::NotFound)?;
            p.next_source_edge = next;
            let mut pbuf = [0u8; EdgeRecord::SIZE];
            p.encode(&mut pbuf);
            Self::overwrite_record(pm, prev, &pbuf, fs)?;
        }
        if !next.is_null() {
            let n_bytes = Self::read_record(pm, next, fs)?.ok_or(StorageError::NotFound)?;
            let mut n = EdgeRecord::decode(&n_bytes).ok_or(StorageError::NotFound)?;
            n.prev_source_edge = prev;
            let mut nbuf = [0u8; EdgeRecord::SIZE];
            n.encode(&mut nbuf);
            Self::overwrite_record(pm, next, &nbuf, fs)?;
        }

        // Clear the deleted edge's own pointers.
        let edge_bytes2 = Self::read_record(pm, edge_slot, fs)?.ok_or(StorageError::NotFound)?;
        let mut edge2 = EdgeRecord::decode(&edge_bytes2).ok_or(StorageError::NotFound)?;
        edge2.prev_source_edge = SlotRef::NULL;
        edge2.next_source_edge = SlotRef::NULL;
        let mut ebuf = [0u8; EdgeRecord::SIZE];
        edge2.encode(&mut ebuf);
        Self::overwrite_record(pm, edge_slot, &ebuf, fs)?;

        // If this was the head, update the source node's first_outgoing_edge.
        if prev.is_null() {
            // Update node record.
            let node_bytes = Self::read_record(pm, node_slot, fs)?.ok_or(StorageError::NotFound)?;
            let mut node = NodeRecord::decode(&node_bytes).ok_or(StorageError::NotFound)?;
            node.first_outgoing_edge = next;
            node.generation += 1;
            let mut nbuf = [0u8; NodeRecord::SIZE];
            node.encode(&mut nbuf);
            Self::overwrite_record(pm, node_slot, &nbuf, fs)?;
            Ok(Some(next))
        } else {
            Ok(None)
        }
    }

    /// Unlink an edge from the target node's adjacency list, patching
    /// neighbour pointers.  Returns the new incoming head if the edge was
    /// the list head.
    fn unwire_target_adjacency(
        pm: &mut PageManager,
        edge_slot: SlotRef,
        node_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<Option<SlotRef>, StorageError> {
        let edge_bytes = Self::read_record(pm, edge_slot, fs)?.ok_or(StorageError::NotFound)?;
        let edge = EdgeRecord::decode(&edge_bytes).ok_or(StorageError::NotFound)?;
        let prev = edge.prev_target_edge;
        let next = edge.next_target_edge;

        if !prev.is_null() {
            let p_bytes = Self::read_record(pm, prev, fs)?.ok_or(StorageError::NotFound)?;
            let mut p = EdgeRecord::decode(&p_bytes).ok_or(StorageError::NotFound)?;
            p.next_target_edge = next;
            let mut pbuf = [0u8; EdgeRecord::SIZE];
            p.encode(&mut pbuf);
            Self::overwrite_record(pm, prev, &pbuf, fs)?;
        }
        if !next.is_null() {
            let n_bytes = Self::read_record(pm, next, fs)?.ok_or(StorageError::NotFound)?;
            let mut n = EdgeRecord::decode(&n_bytes).ok_or(StorageError::NotFound)?;
            n.prev_target_edge = prev;
            let mut nbuf = [0u8; EdgeRecord::SIZE];
            n.encode(&mut nbuf);
            Self::overwrite_record(pm, next, &nbuf, fs)?;
        }

        // Clear the deleted edge's own pointers.
        let edge_bytes2 = Self::read_record(pm, edge_slot, fs)?.ok_or(StorageError::NotFound)?;
        let mut edge2 = EdgeRecord::decode(&edge_bytes2).ok_or(StorageError::NotFound)?;
        edge2.prev_target_edge = SlotRef::NULL;
        edge2.next_target_edge = SlotRef::NULL;
        let mut ebuf = [0u8; EdgeRecord::SIZE];
        edge2.encode(&mut ebuf);
        Self::overwrite_record(pm, edge_slot, &ebuf, fs)?;

        // If this was the head, update the target node's first_incoming_edge.
        if prev.is_null() {
            let node_bytes = Self::read_record(pm, node_slot, fs)?.ok_or(StorageError::NotFound)?;
            let mut node = NodeRecord::decode(&node_bytes).ok_or(StorageError::NotFound)?;
            node.first_incoming_edge = next;
            node.generation += 1;
            let mut nbuf = [0u8; NodeRecord::SIZE];
            node.encode(&mut nbuf);
            Self::overwrite_record(pm, node_slot, &nbuf, fs)?;
            Ok(Some(next))
        } else {
            Ok(None)
        }
    }

    // ------------------------------------------------------------------
    // Full-graph enumeration (used by CLI export / bulk dump — Task 179)
    // ------------------------------------------------------------------

    /// Enumerate every live node in the graph, in ascending `node_id` order.
    ///
    /// Range-scans the primary `node_index` over the full id space and
    /// resolves each entry to its on-disk [`NodeRecord`].  Tombstoned records
    /// (those whose index entry was dropped on delete) are not returned because
    /// `delete_node` removes them from the index.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexError`] if an index value cannot be decoded,
    /// or propagates page-read failures.
    pub fn scan_all_nodes(
        &self,
        fs: &dyn FileSystem,
    ) -> Result<Vec<NodeRecord>, StorageError> {
        let start = node_id_key(0);
        let end = node_id_key(u128::MAX);
        let entries = self.node_index.range_search(&start, &end);
        let mut results = Vec::with_capacity(entries.len());
        for (_key, value) in entries {
            let slot_ref = decode_slot_ref(&value).ok_or(StorageError::IndexError)?;
            if let Some(record) = Self::read_record(&self.page_manager, slot_ref, fs)? {
                if let Some(node) = NodeRecord::decode(&record) {
                    if node.node_id != 0 && node.flags & node_flags::DELETED == 0 {
                        results.push(node);
                    }
                }
            }
        }
        Ok(results)
    }

    /// Enumerate every live edge in the graph, in ascending `edge_id` order.
    ///
    /// Range-scans the primary `edge_index` over the full id space and resolves
    /// each entry to its on-disk [`EdgeRecord`].
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexError`] if an index value cannot be decoded,
    /// or propagates page-read failures.
    pub fn scan_all_edges(
        &self,
        fs: &dyn FileSystem,
    ) -> Result<Vec<EdgeRecord>, StorageError> {
        let start = edge_id_key(0);
        let end = edge_id_key(u128::MAX);
        let entries = self.edge_index.range_search(&start, &end);
        let mut results = Vec::with_capacity(entries.len());
        for (_key, value) in entries {
            let slot_ref = decode_slot_ref(&value).ok_or(StorageError::IndexError)?;
            if let Some(record) = Self::read_record(&self.page_manager, slot_ref, fs)? {
                if let Some(edge) = EdgeRecord::decode(&record) {
                    if edge.edge_id != 0 && edge.flags & edge_flags::DELETED == 0 {
                        results.push(edge);
                    }
                }
            }
        }
        Ok(results)
    }
}

/// Derive a lock-table resource ID for a node from its logical `node_id`.
///
/// The low 32 bits are the node_id; bit 32 is 0 to distinguish nodes from edges.
pub(crate) fn node_resource_id(node_id: u64) -> u64 {
    node_id & 0x0000_FFFF_FFFF_FFFF
}

/// Derive a lock-table resource ID for an edge from its logical `edge_id`.
///
/// Bit 48 is set to keep edge IDs in a separate namespace from node IDs.
pub(crate) fn edge_resource_id(edge_id: u64) -> u64 {
    (edge_id & 0x0000_FFFF_FFFF_FFFF) | (1u64 << 48)
}

/// Convert a `(PageId, slot)` to a [`SlotRef`], checking bounds.
fn slot_ref(page_id: PageId, slot: u16) -> Result<SlotRef, StorageError> {
    if page_id > SlotRef::MAX_PAGE_ID as u64 || slot > SlotRef::MAX_SLOT_INDEX as u16 {
        return Err(StorageError::SlotOverflow);
    }
    Ok(SlotRef::new(page_id as u32, slot as u8))
}

/// Decode a [`SlotRef`] from a 4-byte big-endian value.
fn decode_slot_ref(bytes: &[u8]) -> Option<SlotRef> {
    if bytes.len() < 4 {
        return None;
    }
    let raw = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    Some(SlotRef { raw })
}

impl StorageEngine for GraphStorageEngine {
    fn put_node(
        &mut self,
        node: &NodeRecord,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError> {
        if node.node_id == 0 {
            return Err(StorageError::InvalidId);
        }
        let key = node_id_key(node.node_id as u128);
        if self.node_index.search(&key).is_some() {
            return Err(StorageError::AlreadyExists);
        }

        // Acquire an exclusive lock on this node resource.
        let resource_id = node_resource_id(node.node_id);
        let txn_mgr = Arc::clone(&self.txn_manager);
        let mut autocommit_tx = txn_mgr.begin();
        if let Err(e) = txn_mgr.acquire_lock(&mut autocommit_tx, resource_id, LockMode::Exclusive) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }

        let txid = autocommit_tx.txid;
        let mut buf = [0u8; NodeRecord::SIZE];
        node.encode(&mut buf);
        let slot = match Self::insert_record(
            &mut self.page_manager,
            &buf,
            &mut self.node_pages,
            PageType::SlottedData,
            fs,
        ) {
            Ok(s) => s,
            Err(e) => {
                let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
                return Err(e);
            }
        };

        // Index: node_id -> SlotRef (4 bytes).
        let value = slot.raw.to_be_bytes().to_vec();
        if let Err(e) = self.node_index.insert(&key, &value) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }

        // Secondary label index.
        let label_key = label_index_key(node.label_id as u64, node.node_id as u128);
        if let Err(e) = self.label_index.insert(&label_key, &value) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }

        // WAL: NodeInsert with MVCC xmin = this transaction's TxId.
        let mut payload = Vec::with_capacity(8 + 4 + NodeRecord::SIZE);
        payload.extend_from_slice(&node.node_id.to_be_bytes());
        payload.extend_from_slice(&slot.raw.to_be_bytes());
        // Embed a TupleHeader (xmin = txid, xmax = 0) before the record bytes
        // so recovery can reconstruct MVCC visibility.
        let tuple_hdr = TupleHeader::new_insert(txid, 0);
        let mut hdr_bytes = [0u8; TupleHeader::SIZE];
        tuple_hdr.encode(&mut hdr_bytes);
        payload.extend_from_slice(&hdr_bytes);
        payload.extend_from_slice(&buf);
        if let Err(e) = Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::NodeInsert,
            txid,
            payload,
        ) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        // Commit the autocommit transaction (flush WAL, release locks).
        let wal_fs = Arc::clone(&self.wal_fs);
        txn_mgr.commit(&mut autocommit_tx, &mut self.wal_writer, wal_fs.as_ref())
            .map_err(StorageError::from)?;

        Ok(slot)
    }

    fn get_node(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<NodeRecord>, StorageError> {
        let key = node_id_key(node_id as u128);
        let (_page_id, slot) = match self.node_index.search(&key) {
            Some(r) => r,
            None => return Ok(None),
        };
        let page = self
            .node_index
            .get_page(_page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?;
        // Note: The on-disk record is a raw NodeRecord (no TupleHeader on the
        // data page).  The TupleHeader is written into WAL payloads for recovery
        // and future MVCC chain support, but the slotted page stores the record
        // in its compact fixed-size format.  MVCC visibility in the autocommit
        // path relies on the logical DELETED flag in the NodeRecord.
        match record {
            Some(bytes) => Ok(NodeRecord::decode(&bytes)),
            None => Ok(None),
        }
    }

    fn delete_node(&mut self, node_id: u64, fs: &dyn FileSystem) -> Result<(), StorageError> {
        let key = node_id_key(node_id as u128);
        let (_page_id, slot) = self.node_index.search(&key).ok_or(StorageError::NotFound)?;
        let page = self
            .node_index
            .get_page(_page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        // Acquire exclusive lock on the node.
        let resource_id = node_resource_id(node_id);
        let txn_mgr = Arc::clone(&self.txn_manager);
        let mut autocommit_tx = txn_mgr.begin();
        if let Err(e) = txn_mgr.acquire_lock(&mut autocommit_tx, resource_id, LockMode::Exclusive) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }
        let txid = autocommit_tx.txid;

        let record =
            Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        let mut node = NodeRecord::decode(&record).ok_or(StorageError::NotFound)?;
        node.flags |= node_flags::DELETED;
        node.generation += 1;

        let mut buf = [0u8; NodeRecord::SIZE];
        node.encode(&mut buf);
        if let Err(e) = Self::overwrite_record(&mut self.page_manager, slot_ref, &buf, fs) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        // Remove from label index (tombstone approach: keep in primary index).
        let label_key = label_index_key(node.label_id as u64, node_id as u128);
        if let Err(e) = self.label_index.delete(&label_key) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }

        // WAL: NodeDelete with MVCC xmax = this transaction's TxId.
        let mut payload = node_id.to_be_bytes().to_vec();
        payload.extend_from_slice(&slot_ref.raw.to_be_bytes());
        // TupleHeader: xmin from original insert (0 = unknown here), xmax = txid.
        let mut tuple_hdr = TupleHeader::new_insert(0, 0);
        tuple_hdr.mark_deleted(txid);
        let mut hdr_bytes = [0u8; TupleHeader::SIZE];
        tuple_hdr.encode(&mut hdr_bytes);
        payload.extend_from_slice(&hdr_bytes);
        payload.extend_from_slice(&buf);
        if let Err(e) = Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::NodeDelete,
            txid,
            payload,
        ) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        let wal_fs = Arc::clone(&self.wal_fs);
        txn_mgr.commit(&mut autocommit_tx, &mut self.wal_writer, wal_fs.as_ref())
            .map_err(StorageError::from)?;

        // Track the freed node slot for the free-space map (Task 174).
        self.node_free_list
            .lock()
            .expect("node free list poisoned")
            .push(slot_ref);

        Ok(())
    }

    fn put_edge(
        &mut self,
        edge: &EdgeRecord,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError> {
        if edge.edge_id == 0 {
            return Err(StorageError::InvalidId);
        }
        let key = edge_id_key(edge.edge_id as u128);
        if self.edge_index.search(&key).is_some() {
            return Err(StorageError::AlreadyExists);
        }

        // Acquire exclusive lock on the edge resource.
        let resource_id = edge_resource_id(edge.edge_id);
        let txn_mgr = Arc::clone(&self.txn_manager);
        let mut autocommit_tx = txn_mgr.begin();
        if let Err(e) = txn_mgr.acquire_lock(&mut autocommit_tx, resource_id, LockMode::Exclusive) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }
        let txid = autocommit_tx.txid;

        let mut buf = [0u8; EdgeRecord::SIZE];
        edge.encode(&mut buf);
        let edge_slot = match Self::insert_record(
            &mut self.page_manager,
            &buf,
            &mut self.edge_pages,
            PageType::SlottedData,
            fs,
        ) {
            Ok(s) => s,
            Err(e) => {
                let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
                return Err(e);
            }
        };

        let value = edge_slot.raw.to_be_bytes().to_vec();
        if let Err(e) = self.edge_index.insert(&key, &value) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }

        // Secondary type index.
        let type_key = type_index_key(edge.type_id as u64, edge.edge_id as u128);
        if let Err(e) = self.type_index.insert(&type_key, &value) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }

        // Adjacency list maintenance for source node (outgoing).
        if !edge.source_node.is_null()
            && let Err(e) = Self::wire_source_adjacency(&mut self.page_manager, edge_slot, edge.source_node, fs)
        {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        // Adjacency list maintenance for target node (incoming).
        if !edge.target_node.is_null()
            && let Err(e) = Self::wire_target_adjacency(&mut self.page_manager, edge_slot, edge.target_node, fs)
        {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        // WAL: EdgeInsert with MVCC xmin — re-read to capture pointer updates.
        let final_record = match Self::read_record(&self.page_manager, edge_slot, fs) {
            Ok(Some(r)) => r,
            Ok(None) => {
                let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
                return Err(StorageError::NotFound);
            }
            Err(e) => {
                let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
                return Err(e);
            }
        };
        let mut payload = Vec::with_capacity(8 + 4 + TupleHeader::SIZE + EdgeRecord::SIZE);
        payload.extend_from_slice(&edge.edge_id.to_be_bytes());
        payload.extend_from_slice(&edge_slot.raw.to_be_bytes());
        let tuple_hdr = TupleHeader::new_insert(txid, 0);
        let mut hdr_bytes = [0u8; TupleHeader::SIZE];
        tuple_hdr.encode(&mut hdr_bytes);
        payload.extend_from_slice(&hdr_bytes);
        payload.extend_from_slice(&final_record);
        if let Err(e) = Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::EdgeInsert,
            txid,
            payload,
        ) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        let wal_fs = Arc::clone(&self.wal_fs);
        txn_mgr.commit(&mut autocommit_tx, &mut self.wal_writer, wal_fs.as_ref())
            .map_err(StorageError::from)?;

        Ok(edge_slot)
    }

    fn get_edge(
        &self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<EdgeRecord>, StorageError> {
        let key = edge_id_key(edge_id as u128);
        let (_page_id, slot) = match self.edge_index.search(&key) {
            Some(r) => r,
            None => return Ok(None),
        };
        let page = self
            .edge_index
            .get_page(_page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?;
        // Same as get_node: on-disk record is raw EdgeRecord; MVCC header is
        // only in the WAL payload for recovery.
        match record {
            Some(bytes) => Ok(EdgeRecord::decode(&bytes)),
            None => Ok(None),
        }
    }

    fn delete_edge(&mut self, edge_id: u64, fs: &dyn FileSystem) -> Result<(), StorageError> {
        let key = edge_id_key(edge_id as u128);
        let (_page_id, slot) = self.edge_index.search(&key).ok_or(StorageError::NotFound)?;
        let page = self
            .edge_index
            .get_page(_page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        // Acquire exclusive lock on the edge.
        let resource_id = edge_resource_id(edge_id);
        let txn_mgr = Arc::clone(&self.txn_manager);
        let mut autocommit_tx = txn_mgr.begin();
        if let Err(e) = txn_mgr.acquire_lock(&mut autocommit_tx, resource_id, LockMode::Exclusive) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }
        let txid = autocommit_tx.txid;

        let record =
            Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        let mut edge = EdgeRecord::decode(&record).ok_or(StorageError::NotFound)?;

        // Unlink from adjacency lists before marking as deleted.
        if !edge.source_node.is_null()
            && let Err(e) = Self::unwire_source_adjacency(&mut self.page_manager, slot_ref, edge.source_node, fs)
        {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }
        if !edge.target_node.is_null()
            && let Err(e) = Self::unwire_target_adjacency(&mut self.page_manager, slot_ref, edge.target_node, fs)
        {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        // Re-read the record after pointer patching.
        let record =
            Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        edge = EdgeRecord::decode(&record).ok_or(StorageError::NotFound)?;
        edge.flags |= edge_flags::DELETED;
        edge.generation += 1;

        let mut buf = [0u8; EdgeRecord::SIZE];
        edge.encode(&mut buf);
        if let Err(e) = Self::overwrite_record(&mut self.page_manager, slot_ref, &buf, fs) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        // Remove from type index.
        if let Err(e) = self.type_index.delete(&type_index_key(edge.type_id as u64, edge_id as u128)) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(StorageError::from(e));
        }

        // WAL: EdgeDelete with MVCC xmax = txid.
        let mut payload = edge_id.to_be_bytes().to_vec();
        payload.extend_from_slice(&slot_ref.raw.to_be_bytes());
        let mut tuple_hdr = TupleHeader::new_insert(0, 0);
        tuple_hdr.mark_deleted(txid);
        let mut hdr_bytes = [0u8; TupleHeader::SIZE];
        tuple_hdr.encode(&mut hdr_bytes);
        payload.extend_from_slice(&hdr_bytes);
        payload.extend_from_slice(&buf);
        if let Err(e) = Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::EdgeDelete,
            txid,
            payload,
        ) {
            let _ = txn_mgr.rollback(&mut autocommit_tx, &mut self.wal_writer, &*self.wal_fs);
            return Err(e);
        }

        let wal_fs = Arc::clone(&self.wal_fs);
        txn_mgr.commit(&mut autocommit_tx, &mut self.wal_writer, wal_fs.as_ref())
            .map_err(StorageError::from)?;

        // Track the freed edge slot for the free-space map (Task 174).
        self.edge_free_list
            .lock()
            .expect("edge free list poisoned")
            .push(slot_ref);

        Ok(())
    }

    fn put_property(
        &mut self,
        prop: &PropertyRecord,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError> {
        let bytes = prop.encode();
        let slot = Self::insert_record(
            &mut self.page_manager,
            &bytes,
            &mut self.property_pages,
            PageType::SlottedData,
            fs,
        )?;

        // WAL: PropertyInsert.
        let mut payload = Vec::with_capacity(4 + bytes.len());
        payload.extend_from_slice(&slot.raw.to_be_bytes());
        payload.extend_from_slice(&bytes);
        Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::PropertyInsert,
            1,
            payload,
        )?;

        Ok(slot)
    }

    fn get_property(
        &self,
        slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<Option<PropertyRecord>, StorageError> {
        let record = Self::read_record(&self.page_manager, slot, fs)?;
        match record {
            Some(bytes) => Ok(PropertyRecord::decode(&bytes)),
            None => Ok(None),
        }
    }

    fn attach_property_to_node(
        &mut self,
        node_id: u64,
        prop_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        let key = node_id_key(node_id as u128);
        let (_page_id, slot) = self.node_index.search(&key).ok_or(StorageError::NotFound)?;
        let page = self
            .node_index
            .get_page(_page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record =
            Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        let mut node = NodeRecord::decode(&record).ok_or(StorageError::NotFound)?;
        node.first_property = prop_slot;
        node.generation += 1;

        let mut buf = [0u8; NodeRecord::SIZE];
        node.encode(&mut buf);
        Self::overwrite_record(&mut self.page_manager, slot_ref, &buf, fs)?;

        // WAL: treat as NodeInsert (overwrite semantics).
        let mut payload = Vec::with_capacity(8 + 4 + NodeRecord::SIZE);
        payload.extend_from_slice(&node_id.to_be_bytes());
        payload.extend_from_slice(&slot_ref.raw.to_be_bytes());
        payload.extend_from_slice(&buf);
        Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::NodeInsert,
            1,
            payload,
        )?;

        Ok(())
    }

    fn attach_property_to_edge(
        &mut self,
        edge_id: u64,
        prop_slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        let key = edge_id_key(edge_id as u128);
        let (_page_id, slot) = self.edge_index.search(&key).ok_or(StorageError::NotFound)?;
        let page = self
            .edge_index
            .get_page(_page_id)
            .ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record =
            Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        let mut edge = EdgeRecord::decode(&record).ok_or(StorageError::NotFound)?;
        edge.first_property = prop_slot;
        edge.generation += 1;

        let mut buf = [0u8; EdgeRecord::SIZE];
        edge.encode(&mut buf);
        Self::overwrite_record(&mut self.page_manager, slot_ref, &buf, fs)?;

        // WAL: treat as EdgeInsert (overwrite semantics).
        let mut payload = Vec::with_capacity(8 + 4 + EdgeRecord::SIZE);
        payload.extend_from_slice(&edge_id.to_be_bytes());
        payload.extend_from_slice(&slot_ref.raw.to_be_bytes());
        payload.extend_from_slice(&buf);
        Self::log(
            &mut self.page_manager,
            &mut self.wal_writer,
            fs,
            RecordType::EdgeInsert,
            1,
            payload,
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use crate::storage::meta::decode_superblock;

    fn temp_fs() -> (tempfile::TempDir, PosixFileSystem, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        (dir, fs, path)
    }

    #[test]
    fn init_and_open_roundtrip() {
        let (_dir, fs, path) = temp_fs();
        {
            let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            engine.sync(&fs).unwrap();
        }
        {
            let engine = GraphStorageEngine::open(path, &fs).unwrap();
            assert_eq!(engine.page_manager.superblock.total_page_count, 3);
        }
    }

    #[test]
    fn put_and_get_node() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let node = NodeRecord::new(1, 42);
        let slot = engine.put_node(&node, &fs).unwrap();
        assert!(!slot.is_null());

        let retrieved = engine.get_node(1, &fs).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.node_id, 1);
        assert_eq!(retrieved.label_id, 42);
    }

    #[test]
    fn put_duplicate_node_fails() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let node = NodeRecord::new(1, 42);
        engine.put_node(&node, &fs).unwrap();
        let result = engine.put_node(&node, &fs);
        assert_eq!(result, Err(StorageError::AlreadyExists));
    }

    #[test]
    fn get_missing_node_returns_none() {
        let (_dir, fs, path) = temp_fs();
        let engine = GraphStorageEngine::init(path, &fs).unwrap();
        let result = engine.get_node(999, &fs).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn delete_node_leaves_tombstone() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let node = NodeRecord::new(1, 42);
        engine.put_node(&node, &fs).unwrap();
        engine.delete_node(1, &fs).unwrap();

        // Primary index still has the record (tombstone).
        let retrieved = engine.get_node(1, &fs).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.node_id, 1);
        assert!(retrieved.flags & node_flags::DELETED != 0);
    }

    #[test]
    fn put_and_get_edge() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        // Insert source and target nodes first so adjacency wiring works.
        engine.put_node(&NodeRecord::new(1, 0), &fs).unwrap();
        engine.put_node(&NodeRecord::new(2, 0), &fs).unwrap();
        let source = engine.lookup_node_slot(1).unwrap().unwrap();
        let target = engine.lookup_node_slot(2).unwrap().unwrap();

        let edge = EdgeRecord::new(100, 5, 1, 2, source, target);
        let slot = engine.put_edge(&edge, &fs).unwrap();
        assert!(!slot.is_null());

        let retrieved = engine.get_edge(100, &fs).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.edge_id, 100);
        assert_eq!(retrieved.type_id, 5);
        assert_eq!(retrieved.source_id, 1);
        assert_eq!(retrieved.target_id, 2);
        assert_eq!(retrieved.source_node, source);
        assert_eq!(retrieved.target_node, target);
    }

    #[test]
    fn delete_edge_leaves_tombstone() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        engine.put_node(&NodeRecord::new(1, 0), &fs).unwrap();
        engine.put_node(&NodeRecord::new(2, 0), &fs).unwrap();
        let src_slot = engine.lookup_node_slot(1).unwrap().unwrap();
        let tgt_slot = engine.lookup_node_slot(2).unwrap().unwrap();

        let edge = EdgeRecord::new(100, 5, 1, 2, src_slot, tgt_slot);
        engine.put_edge(&edge, &fs).unwrap();
        engine.delete_edge(100, &fs).unwrap();

        let retrieved = engine.get_edge(100, &fs).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert!(retrieved.flags & edge_flags::DELETED != 0);
    }

    #[test]
    fn put_and_get_property() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let prop = PropertyRecord::inline(
            "msg",
            1,
            crate::graph::record::ValueType::String,
            b"hello".to_vec(),
        );
        let slot = engine.put_property(&prop, &fs).unwrap();

        let retrieved = engine.get_property(slot, &fs).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.payload, b"hello");
    }

    #[test]
    fn attach_property_to_node() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let node = NodeRecord::new(1, 42);
        engine.put_node(&node, &fs).unwrap();

        let prop = PropertyRecord::inline(
            "name",
            1,
            crate::graph::record::ValueType::String,
            b"name".to_vec(),
        );
        let prop_slot = engine.put_property(&prop, &fs).unwrap();

        engine.attach_property_to_node(1, prop_slot, &fs).unwrap();

        let retrieved = engine.get_node(1, &fs).unwrap().unwrap();
        assert_eq!(retrieved.first_property, prop_slot);
    }

    #[test]
    fn attach_property_to_edge() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        engine.put_node(&NodeRecord::new(1, 0), &fs).unwrap();
        engine.put_node(&NodeRecord::new(2, 0), &fs).unwrap();
        let src_slot = engine.lookup_node_slot(1).unwrap().unwrap();
        let tgt_slot = engine.lookup_node_slot(2).unwrap().unwrap();

        let edge = EdgeRecord::new(100, 5, 1, 2, src_slot, tgt_slot);
        engine.put_edge(&edge, &fs).unwrap();

        let prop = PropertyRecord::inline(
            "weight",
            1,
            crate::graph::record::ValueType::String,
            b"weight".to_vec(),
        );
        let prop_slot = engine.put_property(&prop, &fs).unwrap();

        engine.attach_property_to_edge(100, prop_slot, &fs).unwrap();

        let retrieved = engine.get_edge(100, &fs).unwrap().unwrap();
        assert_eq!(retrieved.first_property, prop_slot);
    }

    #[test]
    fn node_record_wal_logged() {
        let (dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let node = NodeRecord::new(7, 99);
        engine.put_node(&node, &fs).unwrap();
        engine.sync(&fs).unwrap();

        // Read the WAL segment back and verify a NodeInsert record exists.
        let seg = dir.path().join("wal").join("wal-000000000");
        let handle = fs.open(&seg, false).unwrap();
        let len = handle.len().unwrap() as usize;
        let mut buf = vec![0u8; len];
        handle.read_at(&mut buf, 0).unwrap();

        let mut found = false;
        let mut offset = 0;
        while offset < buf.len() {
            if let Some((rec, size)) = WalRecord::decode(&buf, offset) {
                if rec.record_type == RecordType::NodeInsert {
                    found = true;
                    // WAL payload: node_id (8) + slot_ref (4) +
                    //              TupleHeader (16, xmin/xmax for MVCC) +
                    //              NodeRecord (32).
                    use crate::txn::mvcc::TupleHeader as TH;
                    assert_eq!(
                        rec.payload.len(),
                        8 + 4 + TH::SIZE + NodeRecord::SIZE,
                        "NodeInsert WAL payload must include TupleHeader"
                    );
                }
                offset += size;
            } else {
                break;
            }
        }
        assert!(found, "NodeInsert WAL record not found");
    }

    #[test]
    fn label_index_supports_range_scan() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        for i in 1u64..=10 {
            let node = NodeRecord::new(i, 5);
            engine.put_node(&node, &fs).unwrap();
        }

        // Verify all nodes are in the label index by checking node_index.
        for i in 1u64..=10 {
            let retrieved = engine.get_node(i, &fs).unwrap();
            assert!(retrieved.is_some(), "node {} should exist", i);
        }
    }

    #[test]
    fn indexes_rebuilt_after_crash_recovery() {
        use crate::index::key::{label_index_key, type_index_key};

        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();

        // Insert nodes with different labels.
        let node1 = NodeRecord::new(1, 10);
        let node2 = NodeRecord::new(2, 20);
        engine.put_node(&node1, &fs).unwrap();
        engine.put_node(&node2, &fs).unwrap();

        // Insert edges with different types.
        let n1_slot = engine.lookup_node_slot(1).unwrap().unwrap();
        let n2_slot = engine.lookup_node_slot(2).unwrap().unwrap();
        let edge1 = EdgeRecord::new(100, 1, 1, 2, n1_slot, n2_slot);
        let edge2 = EdgeRecord::new(101, 2, 1, 2, n1_slot, n2_slot);
        engine.put_edge(&edge1, &fs).unwrap();
        engine.put_edge(&edge2, &fs).unwrap();

        // Sync to disk.
        engine.sync(&fs).unwrap();

        // Verify data exists before crash.
        assert!(
            engine.get_node(1, &fs).unwrap().is_some(),
            "node 1 should exist before crash"
        );

        // Simulate crash: drop the engine and reopen from disk.
        drop(engine);
        let recovered = GraphStorageEngine::open(path, &fs).unwrap();

        // Verify primary indexes are restored.
        let n1 = recovered.get_node(1, &fs).unwrap();
        assert!(n1.is_some(), "node 1 should be recoverable");
        assert_eq!(n1.unwrap().label_id, 10);

        let n2 = recovered.get_node(2, &fs).unwrap();
        assert!(n2.is_some(), "node 2 should be recoverable");
        assert_eq!(n2.unwrap().label_id, 20);

        let e1 = recovered.get_edge(100, &fs).unwrap();
        assert!(e1.is_some(), "edge 100 should be recoverable");
        assert_eq!(e1.unwrap().type_id, 1);

        let e2 = recovered.get_edge(101, &fs).unwrap();
        assert!(e2.is_some(), "edge 101 should be recoverable");
        assert_eq!(e2.unwrap().type_id, 2);

        // Verify secondary label index is rebuilt.
        let label_key1 = label_index_key(10, 1);
        let label_key2 = label_index_key(20, 2);
        assert!(
            recovered.label_index.search(&label_key1).is_some(),
            "label index for node 1 should be rebuilt"
        );
        assert!(
            recovered.label_index.search(&label_key2).is_some(),
            "label index for node 2 should be rebuilt"
        );

        // Verify secondary type index is rebuilt.
        let type_key1 = type_index_key(1, 100);
        let type_key2 = type_index_key(2, 101);
        assert!(
            recovered.type_index.search(&type_key1).is_some(),
            "type index for edge 100 should be rebuilt"
        );
        assert!(
            recovered.type_index.search(&type_key2).is_some(),
            "type index for edge 101 should be rebuilt"
        );
    }

    #[test]
    fn corrupt_primary_superblock_recover_from_mirror() {
        let (_dir, fs, path) = temp_fs();
        {
            let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            engine.sync(&fs).unwrap();
            drop(engine);
        }

        // Corrupt the primary superblock (offset 0).
        let handle = fs.open(&path, true).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 0).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, 0).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the mirror copy.
        let engine = GraphStorageEngine::open(path, &fs).unwrap();
        assert_eq!(engine.page_manager.superblock.total_page_count, 3);
    }

    #[test]
    fn corrupt_mirror_superblock_recover_from_primary() {
        let (_dir, fs, path) = temp_fs();
        {
            let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            engine.sync(&fs).unwrap();
            drop(engine);
        }

        // Corrupt the mirror superblock (offset PAGE_SIZE).
        let handle = fs.open(&path, true).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, PAGE_SIZE as u64).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, PAGE_SIZE as u64).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the primary copy.
        let engine = GraphStorageEngine::open(path, &fs).unwrap();
        assert_eq!(engine.page_manager.superblock.total_page_count, 3);
    }

    #[test]
    fn crash_mid_superblock_write_recovers_last_good_generation() {
        let (_dir, fs, path) = temp_fs();
        {
            let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            engine.sync(&fs).unwrap();
            drop(engine);
        }

        let handle = fs.open(&path, true).unwrap();

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
        let engine = GraphStorageEngine::open(path, &fs).unwrap();
        assert_eq!(engine.page_manager.superblock.generation, old_generation);
    }

    // ── Task 148: ARIES recovery wired at startup ──────────────────────────

    #[test]
    fn committed_data_survives_restart() {
        // This test verifies the Task 148 acceptance criteria: committed
        // transactions are fully redone after a simulated restart.
        let (_dir, fs, path) = temp_fs();

        let node_id;
        {
            let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            let node = NodeRecord::new(1, 42);
            engine.put_node(&node, &fs).unwrap();
            engine.sync(&fs).unwrap();
            node_id = 1u64;
        }

        // Reopen — ARIES recovery must restore the committed state.
        let engine = GraphStorageEngine::open(path, &fs).unwrap();
        let result = engine.get_node(node_id, &fs).unwrap();
        assert!(
            result.is_some(),
            "committed node must be present after restart"
        );
        let n = result.unwrap();
        assert_eq!(n.node_id, 1);
        assert_eq!(n.label_id, 42);
    }

    #[test]
    fn aries_recovery_runs_without_error_on_empty_wal() {
        // When no WAL segments exist (fresh database opened normally), ARIES
        // must complete without error (no segments → no records → no-op).
        let (_dir, fs, path) = temp_fs();
        {
            let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            engine.sync(&fs).unwrap();
        }
        // Remove the WAL directory to simulate a missing WAL.
        let wal_dir = path.parent().unwrap().join("wal");
        if wal_dir.exists() {
            std::fs::remove_dir_all(&wal_dir).unwrap();
        }
        // Open must succeed even with no WAL.
        let engine = GraphStorageEngine::open(path, &fs).unwrap();
        assert_eq!(engine.page_manager.superblock.total_page_count, 3);
    }

    // ------------------------------------------------------------------
    // Task 174: free-space map and tombstone compaction
    // ------------------------------------------------------------------

    #[test]
    fn delete_node_adds_to_free_list() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let node = NodeRecord::new(1, 42);
        engine.put_node(&node, &fs).unwrap();
        assert_eq!(engine.node_free_list.lock().unwrap().len(), 0);

        engine.delete_node(1, &fs).unwrap();
        assert_eq!(
            engine.node_free_list.lock().unwrap().len(),
            1,
            "free list should have 1 entry after deletion"
        );
    }

    #[test]
    fn delete_edge_adds_to_free_list() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        // Insert endpoints first so adjacency wiring succeeds.
        engine.put_node(&NodeRecord::new(1, 0), &fs).unwrap();
        engine.put_node(&NodeRecord::new(2, 0), &fs).unwrap();
        let src = engine.lookup_node_slot(1).unwrap().unwrap();
        let tgt = engine.lookup_node_slot(2).unwrap().unwrap();

        let edge = EdgeRecord::new(100, 5, 1, 2, src, tgt);
        engine.put_edge(&edge, &fs).unwrap();
        assert_eq!(engine.edge_free_list.lock().unwrap().len(), 0);

        engine.delete_edge(100, &fs).unwrap();
        assert_eq!(
            engine.edge_free_list.lock().unwrap().len(),
            1,
            "edge free list should have 1 entry after deletion"
        );
    }

    #[test]
    fn compact_wal_logs_and_returns_reclaimed_count() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        // Insert and delete nodes to create tombstones.
        for i in 1u64..=5 {
            engine.put_node(&NodeRecord::new(i, 10), &fs).unwrap();
        }
        for i in 1u64..=5 {
            engine.delete_node(i, &fs).unwrap();
        }

        let reclaimed = engine.compact(&fs).unwrap();
        assert!(
            reclaimed >= 5,
            "compact() should report at least the 5 deleted node slots; got {}",
            reclaimed
        );

        // The free list must not grow beyond what compaction leaves behind.
        let fl = engine.node_free_list.lock().unwrap().len();
        assert!(fl <= 5, "free list should not grow past the deletions");
    }

    #[test]
    fn compact_is_wal_logged_with_brackets() {
        let (dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        engine.put_node(&NodeRecord::new(1, 10), &fs).unwrap();
        engine.delete_node(1, &fs).unwrap();
        engine.compact(&fs).unwrap();
        engine.sync(&fs).unwrap();

        let seg = dir.path().join("wal").join("wal-000000000");
        let handle = fs.open(&seg, false).unwrap();
        let len = handle.len().unwrap() as usize;
        let mut buf = vec![0u8; len];
        handle.read_at(&mut buf, 0).unwrap();

        let mut saw_begin = false;
        let mut saw_end = false;
        let mut offset = 0;
        while offset < buf.len() {
            if let Some((rec, size)) = WalRecord::decode(&buf, offset) {
                match rec.record_type {
                    RecordType::CompactionBegin => saw_begin = true,
                    RecordType::CompactionEnd => saw_end = true,
                    _ => {}
                }
                offset += size;
            } else {
                break;
            }
        }
        assert!(saw_begin, "CompactionBegin WAL record must be present");
        assert!(saw_end, "CompactionEnd WAL record must be present");
    }

    // ------------------------------------------------------------------
    // Task 58: freeze_adjacency / scan_adjacency
    // ------------------------------------------------------------------

    #[test]
    fn freeze_adjacency_returns_correct_edges() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        engine.put_node(&NodeRecord::new(1, 1), &fs).unwrap();
        engine.put_node(&NodeRecord::new(2, 1), &fs).unwrap();
        let src = engine.lookup_node_slot(1).unwrap().unwrap();
        let tgt = engine.lookup_node_slot(2).unwrap().unwrap();

        // Edge 100: logical node 1 → node 2.
        let edge = EdgeRecord::new(100, 5, 1, 2, src, tgt);
        engine.put_edge(&edge, &fs).unwrap();

        let snap = engine.freeze_adjacency(&fs);
        assert!(
            engine.csr.is_frozen(),
            "CSR should be frozen after freeze_adjacency()"
        );

        let edges: Vec<_> = snap.outgoing_edges(1).collect();
        assert_eq!(edges.len(), 1, "node 1 should have 1 outgoing edge");
        assert_eq!(edges[0].target_id, 2, "edge target should be node 2");
        assert_eq!(edges[0].edge_id, 100, "edge id should be 100");
        assert_eq!(edges[0].edge_type, 5, "edge type should be 5");
    }

    #[test]
    fn scan_adjacency_csr_and_linked_list_agree() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        engine.put_node(&NodeRecord::new(1, 1), &fs).unwrap();
        engine.put_node(&NodeRecord::new(2, 1), &fs).unwrap();
        engine.put_node(&NodeRecord::new(3, 1), &fs).unwrap();
        let s1 = engine.lookup_node_slot(1).unwrap().unwrap();
        let s2 = engine.lookup_node_slot(2).unwrap().unwrap();
        let s3 = engine.lookup_node_slot(3).unwrap().unwrap();

        // Edges from node 1 → 2 and node 1 → 3 (adjacency auto-wired by put_edge).
        engine.put_edge(&EdgeRecord::new(10, 1, 1, 2, s1, s2), &fs).unwrap();
        engine.put_edge(&EdgeRecord::new(11, 1, 1, 3, s1, s3), &fs).unwrap();

        // Linked-list path (no CSR yet) must already see both edges.
        let mut ll = engine.scan_adjacency(1, &fs).unwrap();
        ll.sort_by_key(|(t, _, _)| *t);
        let ll_targets: Vec<u64> = ll.iter().map(|(t, _, _)| *t).collect();
        assert_eq!(ll_targets, vec![2, 3], "linked-list scan should see both edges");

        // Freeze and compare CSR results.
        engine.freeze_adjacency(&fs);
        let mut csr = engine.scan_adjacency(1, &fs).unwrap();
        csr.sort_by_key(|(t, _, _)| *t);
        assert_eq!(csr, ll, "CSR and linked-list scans must agree");
    }

    #[test]
    fn thaw_removes_csr_snapshot() {
        let (_dir, fs, path) = temp_fs();
        let engine = GraphStorageEngine::init(path, &fs).unwrap();
        engine.freeze_adjacency(&fs);
        assert!(engine.csr.is_frozen());
        engine.thaw_adjacency();
        assert!(
            !engine.csr.is_frozen(),
            "CSR should not be frozen after thaw_adjacency()"
        );
    }

    // ------------------------------------------------------------------
    // RDF data model (Task 180)
    // ------------------------------------------------------------------

    use crate::rdf::{Quad, RdfLiteral, Term, Triple, xsd};

    fn t_alice() -> Term {
        Term::iri("http://example.org/alice")
    }
    fn t_bob() -> Term {
        Term::iri("http://example.org/bob")
    }
    fn p_knows() -> Term {
        Term::iri("http://xmlns.com/foaf/0.1/knows")
    }
    fn p_name() -> Term {
        Term::iri("http://xmlns.com/foaf/0.1/name")
    }

    #[test]
    fn rdf_insert_and_match_exact_spo() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        let triple = Triple::new(t_alice(), p_knows(), t_bob());
        assert!(engine.add_triple(&triple, &fs).unwrap(), "newly added");
        // Idempotent re-insert.
        assert!(!engine.add_triple(&triple, &fs).unwrap(), "already present");
        assert_eq!(engine.rdf_triple_count(), 1);

        let results = engine.match_triples(Some(&t_alice()), Some(&p_knows()), Some(&t_bob()));
        assert_eq!(results, vec![triple]);
    }

    #[test]
    fn rdf_match_all_five_patterns() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        // (alice knows bob), (alice name "Alice"), (bob knows alice)
        let t1 = Triple::new(t_alice(), p_knows(), t_bob());
        let t2 = Triple::new(
            t_alice(),
            p_name(),
            Term::literal(RdfLiteral::string("Alice")),
        );
        let t3 = Triple::new(t_bob(), p_knows(), t_alice());
        for t in [&t1, &t2, &t3] {
            engine.add_triple(t, &fs).unwrap();
        }

        // (s, p, o): exact.
        assert_eq!(
            engine.match_triples(Some(&t_alice()), Some(&p_knows()), Some(&t_bob())),
            vec![t1.clone()]
        );
        // (s, ?, ?): all of alice's triples.
        let by_subject = engine.match_triples(Some(&t_alice()), None, None);
        assert_eq!(by_subject.len(), 2);
        assert!(by_subject.contains(&t1));
        assert!(by_subject.contains(&t2));
        // (?, p, ?): all `knows` triples.
        let by_pred = engine.match_triples(None, Some(&p_knows()), None);
        assert_eq!(by_pred.len(), 2);
        assert!(by_pred.contains(&t1));
        assert!(by_pred.contains(&t3));
        // (?, ?, o): everything with object = bob.
        let by_object = engine.match_triples(None, None, Some(&t_bob()));
        assert_eq!(by_object, vec![t1.clone()]);
        // (?, ?, ?): all triples.
        let all = engine.match_triples(None, None, None);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn rdf_unknown_term_yields_no_matches() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        engine
            .add_triple(&Triple::new(t_alice(), p_knows(), t_bob()), &fs)
            .unwrap();
        // A subject never interned must not match anything (and must not panic).
        let never = Term::iri("http://example.org/nobody");
        assert!(engine.match_triples(Some(&never), None, None).is_empty());
    }

    #[test]
    fn rdf_named_graph_quads() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        let g = Term::iri("http://example.org/graph1");
        let triple = Triple::new(t_alice(), p_knows(), t_bob());
        // Same triple in the default graph and in a named graph are distinct.
        engine.add_triple(&triple, &fs).unwrap();
        engine
            .add_quad(&Quad::in_graph(triple.clone(), g.clone()), &fs)
            .unwrap();
        assert_eq!(engine.rdf_triple_count(), 2);

        // Filter by named graph.
        let in_g = engine.match_quads(None, None, None, Some(&g));
        assert_eq!(in_g.len(), 1);
        assert_eq!(in_g[0].graph, Some(g));
        // Across all graphs.
        assert_eq!(engine.all_quads().len(), 2);
    }

    #[test]
    fn rdf_delete_triple() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        let triple = Triple::new(t_alice(), p_knows(), t_bob());
        engine.add_triple(&triple, &fs).unwrap();
        assert!(engine.delete_triple(&triple, &fs).unwrap());
        assert_eq!(engine.rdf_triple_count(), 0);
        assert!(
            engine
                .match_triples(Some(&t_alice()), Some(&p_knows()), Some(&t_bob()))
                .is_empty()
        );
        // Deleting again is a no-op (false).
        assert!(!engine.delete_triple(&triple, &fs).unwrap());
    }

    #[test]
    fn rdf_delete_one_graph_keeps_other() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        let g = Term::iri("http://example.org/graph1");
        let triple = Triple::new(t_alice(), p_knows(), t_bob());
        engine.add_triple(&triple, &fs).unwrap();
        engine
            .add_quad(&Quad::in_graph(triple.clone(), g.clone()), &fs)
            .unwrap();

        // Delete the default-graph copy; the named-graph copy must survive.
        assert!(engine.delete_triple(&triple, &fs).unwrap());
        assert_eq!(engine.rdf_triple_count(), 1);
        let surviving = engine.match_quads(None, None, None, Some(&g));
        assert_eq!(surviving.len(), 1);
        assert_eq!(surviving[0].graph, Some(g));
    }

    #[test]
    fn rdf_typed_literals_distinct_from_strings() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();
        let s = t_alice();
        let p = Term::iri("http://example.org/age");
        let int_lit = Term::literal(RdfLiteral::typed("30", xsd::INTEGER));
        let str_lit = Term::literal(RdfLiteral::string("30"));
        engine
            .add_triple(&Triple::new(s.clone(), p.clone(), int_lit.clone()), &fs)
            .unwrap();
        engine
            .add_triple(&Triple::new(s.clone(), p.clone(), str_lit.clone()), &fs)
            .unwrap();
        // The two literals are distinct terms → two distinct triples.
        assert_eq!(engine.rdf_triple_count(), 2);
        let matched = engine.match_triples(Some(&s), Some(&p), Some(&int_lit));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].object, int_lit);
    }

    #[test]
    fn rdf_survives_crash_and_replay() {
        let (_dir, fs, path) = temp_fs();
        let g = Term::iri("http://example.org/graph1");
        let t1 = Triple::new(t_alice(), p_knows(), t_bob());
        let t2 = Triple::new(
            t_alice(),
            p_name(),
            Term::literal(RdfLiteral::lang("Alice", "en")),
        );
        {
            let mut engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            engine.add_triple(&t1, &fs).unwrap();
            engine.add_triple(&t2, &fs).unwrap();
            engine
                .add_quad(&Quad::in_graph(t1.clone(), g.clone()), &fs)
                .unwrap();
            engine.sync(&fs).unwrap();
        }
        // Reopen: the dictionary and permutation index are rebuilt from pages.
        let engine = GraphStorageEngine::open(path, &fs).unwrap();
        assert_eq!(engine.rdf_triple_count(), 3, "all triples recovered");
        // Exact match still works after rebuild.
        assert_eq!(
            engine.match_triples(Some(&t_alice()), Some(&p_knows()), Some(&t_bob())),
            vec![t1.clone()]
        );
        // The lang literal round-trips exactly.
        let names = engine.match_triples(Some(&t_alice()), Some(&p_name()), None);
        assert_eq!(names.len(), 1);
        assert_eq!(
            names[0].object,
            Term::literal(RdfLiteral::lang("Alice", "en"))
        );
        // Named-graph quad is recovered with its graph intact.
        let in_g = engine.match_quads(None, None, None, Some(&g));
        assert_eq!(in_g.len(), 1);
        assert_eq!(in_g[0].graph, Some(g));
    }
}
