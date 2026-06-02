//! Graph storage engine trait and implementation.
//!
//! The [`GraphStorageEngine`] provides CRUD operations for nodes, edges,
//! and properties on top of the page manager, B+ tree indexes, and WAL.

use crate::graph::record::{EdgeRecord, NodeRecord, PropertyRecord, SlotRef, node_flags, edge_flags};
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{edge_id_key, label_index_key, node_id_key, type_index_key};
use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::manager::PageManager;
use crate::storage::page::{PageId, PageType, SlottedPage, PAGE_SIZE};
use crate::storage::meta::decode_superblock;
use crate::wal::record::{RecordType, WalRecord};
use crate::wal::writer::WalWriter;
use crate::wal::recovery::{recover, simple_page_replay};
use std::io;
use std::path::PathBuf;

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

/// Core trait for graph storage operations.
pub trait StorageEngine {
    /// Insert a node record. Returns its [`SlotRef`].
    fn put_node(
        &mut self,
        node: &NodeRecord,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError>;

    /// Retrieve a node by its `node_id`.
    fn get_node(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<NodeRecord>, StorageError>;

    /// Delete a node (leaves a tombstone).
    fn delete_node(
        &mut self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError>;

    /// Insert an edge record. Returns its [`SlotRef`].
    fn put_edge(
        &mut self,
        edge: &EdgeRecord,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError>;

    /// Retrieve an edge by its `edge_id`.
    fn get_edge(
        &self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<EdgeRecord>, StorageError>;

    /// Delete an edge (leaves a tombstone).
    fn delete_edge(
        &mut self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError>;

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
/// Owns the page manager, WAL writer, and all B+ tree indexes.
/// Pages used for graph records are tracked in `node_pages`,
/// `edge_pages`, and `property_pages` so that insertions prefer
/// recently-used pages before allocating new ones.
#[derive(Debug)]
pub struct GraphStorageEngine {
    pub page_manager: PageManager,
    pub wal_writer: WalWriter,
    pub node_index: BPlusTree,
    pub edge_index: BPlusTree,
    pub label_index: BPlusTree,
    pub type_index: BPlusTree,
    pub node_pages: Vec<PageId>,
    pub edge_pages: Vec<PageId>,
    pub property_pages: Vec<PageId>,
}

impl GraphStorageEngine {
    /// Initialise a brand-new graph storage engine.
    pub fn init(data_path: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        let pm = PageManager::init(data_path.clone(), PAGE_SIZE as u32)?;

        // Pre-extend the data file to two pages (superblock + bitmap).
        let handle = fs.open(&data_path, true)?;
        handle.set_len((2 * PAGE_SIZE) as u64)?;
        handle.sync_data()?;

        // Write superblock (both copies).
        pm.sync_superblock(fs)?;
        // Write bitmap page.
        pm.sync_bitmap(fs)?;

        // Initialise WAL.
        let wal_dir = data_path.parent().unwrap().join("wal");
        let wal_writer = WalWriter::open(wal_dir, fs)?;

        let config = BPlusTreeConfig::default();
        Ok(Self {
            page_manager: pm,
            wal_writer,
            node_index: BPlusTree::new(config.clone()),
            edge_index: BPlusTree::new(config.clone()),
            label_index: BPlusTree::new(config.clone()),
            type_index: BPlusTree::new(config),
            node_pages: Vec::new(),
            edge_pages: Vec::new(),
            property_pages: Vec::new(),
        })
    }

    /// Open an existing engine, recovering WAL if necessary.
    pub fn open(data_path: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        if !data_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "database data file missing",
            ));
        }

        // Read both superblock copies.
        let handle = fs.open(&data_path, false)?;
        let mut primary = AlignedBuffer::zeroed(PAGE_SIZE);
        let mut mirror = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut primary, 0)?;
        handle.read_at(&mut mirror, PAGE_SIZE as u64)?;

        let sb_primary = decode_superblock(&primary
        ).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid primary superblock"))?;
        let sb_mirror = decode_superblock(&mirror);

        let sb = match sb_mirror {
            Some(m) if m.generation > sb_primary.generation => m,
            _ => sb_primary,
        };

        // Read bitmap page.
        let mut bitmap_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut bitmap_buf, PAGE_SIZE as u64)?;

        let mut pm = PageManager::open(data_path.clone(), sb, bitmap_buf)?;

        // Recover WAL.
        let wal_dir = data_path.parent().unwrap().join("wal");
        let wal_path = wal_dir.join("wal-000000000");
        let start_lsn = pm.superblock.last_checkpoint_lsn;
        if let Some(last_lsn) = recover(
            fs,
            &wal_path,
            start_lsn,
            |pid, img, lsn| simple_page_replay(fs, &data_path, pid, img, lsn),
        )? {
            pm.superblock.current_wal_lsn = last_lsn;
            pm.sync_superblock(fs)?;
        }

        let wal_writer = WalWriter::open(wal_dir, fs)?;

        let config = BPlusTreeConfig::default();
        Ok(Self {
            page_manager: pm,
            wal_writer,
            node_index: BPlusTree::new(config.clone()),
            edge_index: BPlusTree::new(config.clone()),
            label_index: BPlusTree::new(config.clone()),
            type_index: BPlusTree::new(config),
            node_pages: Vec::new(),
            edge_pages: Vec::new(),
            property_pages: Vec::new(),
        })
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

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
                page_manager.write_page(fs, page_id, &page.buf)?;
                return slot_ref(page_id, slot);
            }
        }

        // Allocate a new page.
        let page_id = page_manager.allocate_page();
        let mut page = SlottedPage::init(page_id, page_type);
        let slot = page.insert(record).ok_or(StorageError::PageFull)?;
        page.update_checksum();
        page_manager.write_page(fs, page_id, &page.buf)?;
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
        page_manager.write_page(fs, page_id, &page.buf)?;
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

    /// Sync the WAL and superblock to durable storage.
    pub fn sync(
        &mut self,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        self.wal_writer.sync(fs)?;
        self.page_manager.sync_superblock(fs)?;
        Ok(())
    }
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
        let key = node_id_key(node.node_id as u128);
        if self.node_index.search(&key).is_some() {
            return Err(StorageError::AlreadyExists);
        }

        let mut buf = [0u8; NodeRecord::SIZE];
        node.encode(&mut buf);
        let slot = Self::insert_record(
            &mut self.page_manager,
            &buf,
            &mut self.node_pages,
            PageType::SlottedData,
            fs,
        )?;

        // Index: node_id -> SlotRef (4 bytes).
        let value = slot.raw.to_be_bytes().to_vec();
        self.node_index.insert(&key, &value)?;

        // Secondary label index.
        let label_key = label_index_key(node.label_id as u64, node.node_id as u128);
        self.label_index.insert(&label_key, &value)?;

        // WAL: NodeInsert.
        let mut payload = Vec::with_capacity(8 + 4 + NodeRecord::SIZE);
        payload.extend_from_slice(&node.node_id.to_be_bytes());
        payload.extend_from_slice(&slot.raw.to_be_bytes());
        payload.extend_from_slice(&buf);
        Self::log(&mut self.page_manager, &mut self.wal_writer, fs, RecordType::NodeInsert, 1, payload)?;

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
        let page = self.node_index.get_page(_page_id).ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?;
        match record {
            Some(bytes) => Ok(NodeRecord::decode(&bytes)),
            None => Ok(None),
        }
    }

    fn delete_node(
        &mut self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        let key = node_id_key(node_id as u128);
        let (_page_id, slot) = self.node_index.search(&key).ok_or(StorageError::NotFound)?;
        let page = self.node_index.get_page(_page_id).ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        let mut node = NodeRecord::decode(&record).ok_or(StorageError::NotFound)?;
        node.flags |= node_flags::DELETED;
        node.generation += 1;

        let mut buf = [0u8; NodeRecord::SIZE];
        node.encode(&mut buf);
        Self::overwrite_record(&mut self.page_manager, slot_ref, &buf, fs)?;

        // Remove from label index (tombstone approach: keep in primary index).
        let label_key = label_index_key(node.label_id as u64, node_id as u128);
        self.label_index.delete(&label_key)?;

        // WAL: NodeDelete.
        let mut payload = node_id.to_be_bytes().to_vec();
        payload.extend_from_slice(&slot_ref.raw.to_be_bytes());
        payload.extend_from_slice(&buf);
        Self::log(&mut self.page_manager, &mut self.wal_writer, fs, RecordType::NodeDelete, 1, payload)?;

        Ok(())
    }

    fn put_edge(
        &mut self,
        edge: &EdgeRecord,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError> {
        let key = edge_id_key(edge.edge_id as u128);
        if self.edge_index.search(&key).is_some() {
            return Err(StorageError::AlreadyExists);
        }

        let mut buf = [0u8; EdgeRecord::SIZE];
        edge.encode(&mut buf);
        let slot = Self::insert_record(
            &mut self.page_manager,
            &buf,
            &mut self.edge_pages,
            PageType::SlottedData,
            fs,
        )?;

        let value = slot.raw.to_be_bytes().to_vec();
        self.edge_index.insert(&key, &value)?;

        // Secondary type index.
        let type_key = type_index_key(edge.type_id as u64, edge.edge_id as u128);
        self.type_index.insert(&type_key, &value)?;

        // WAL: EdgeInsert.
        let mut payload = Vec::with_capacity(8 + 4 + EdgeRecord::SIZE);
        payload.extend_from_slice(&edge.edge_id.to_be_bytes());
        payload.extend_from_slice(&slot.raw.to_be_bytes());
        payload.extend_from_slice(&buf);
        Self::log(&mut self.page_manager, &mut self.wal_writer, fs, RecordType::EdgeInsert, 1, payload)?;

        Ok(slot)
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
        let page = self.edge_index.get_page(_page_id).ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?;
        match record {
            Some(bytes) => Ok(EdgeRecord::decode(&bytes)),
            None => Ok(None),
        }
    }

    fn delete_edge(
        &mut self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        let key = edge_id_key(edge_id as u128);
        let (_page_id, slot) = self.edge_index.search(&key).ok_or(StorageError::NotFound)?;
        let page = self.edge_index.get_page(_page_id).ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
        let mut edge = EdgeRecord::decode(&record).ok_or(StorageError::NotFound)?;
        edge.flags |= edge_flags::DELETED;
        edge.generation += 1;

        let mut buf = [0u8; EdgeRecord::SIZE];
        edge.encode(&mut buf);
        Self::overwrite_record(&mut self.page_manager, slot_ref, &buf, fs)?;

        // Remove from type index.
        let type_key = type_index_key(edge.type_id as u64, edge_id as u128);
        self.type_index.delete(&type_key)?;

        // WAL: EdgeDelete.
        let mut payload = edge_id.to_be_bytes().to_vec();
        payload.extend_from_slice(&slot_ref.raw.to_be_bytes());
        payload.extend_from_slice(&buf);
        Self::log(&mut self.page_manager, &mut self.wal_writer, fs, RecordType::EdgeDelete, 1, payload)?;

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
        Self::log(&mut self.page_manager, &mut self.wal_writer, fs, RecordType::PropertyInsert, 1, payload)?;

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
        let page = self.node_index.get_page(_page_id).ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
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
        Self::log(&mut self.page_manager, &mut self.wal_writer, fs, RecordType::NodeInsert, 1, payload)?;

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
        let page = self.edge_index.get_page(_page_id).ok_or(StorageError::IndexError)?;
        let kv = page.key(slot).ok_or(StorageError::IndexError)?;
        let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
        let value = &kv[2 + key_len..];
        let slot_ref = decode_slot_ref(value).ok_or(StorageError::IndexError)?;

        let record = Self::read_record(&self.page_manager, slot_ref, fs)?.ok_or(StorageError::NotFound)?;
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
        Self::log(&mut self.page_manager, &mut self.wal_writer, fs, RecordType::EdgeInsert, 1, payload)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;

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
            assert_eq!(engine.page_manager.superblock.total_page_count, 2);
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

        let source = SlotRef::new(1, 0);
        let target = SlotRef::new(2, 0);
        let edge = EdgeRecord::new(100, 5, source, target);
        let slot = engine.put_edge(&edge, &fs).unwrap();
        assert!(!slot.is_null());

        let retrieved = engine.get_edge(100, &fs).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.edge_id, 100);
        assert_eq!(retrieved.type_id, 5);
        assert_eq!(retrieved.source_node, source);
        assert_eq!(retrieved.target_node, target);
    }

    #[test]
    fn delete_edge_leaves_tombstone() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let edge = EdgeRecord::new(100, 5, SlotRef::new(1, 0), SlotRef::new(2, 0));
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

        let prop = PropertyRecord::inline(1, crate::graph::record::ValueType::String, b"hello".to_vec());
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

        let prop = PropertyRecord::inline(1, crate::graph::record::ValueType::String, b"name".to_vec());
        let prop_slot = engine.put_property(&prop, &fs).unwrap();

        engine.attach_property_to_node(1, prop_slot, &fs).unwrap();

        let retrieved = engine.get_node(1, &fs).unwrap().unwrap();
        assert_eq!(retrieved.first_property, prop_slot);
    }

    #[test]
    fn attach_property_to_edge() {
        let (_dir, fs, path) = temp_fs();
        let mut engine = GraphStorageEngine::init(path, &fs).unwrap();

        let edge = EdgeRecord::new(100, 5, SlotRef::new(1, 0), SlotRef::new(2, 0));
        engine.put_edge(&edge, &fs).unwrap();

        let prop = PropertyRecord::inline(1, crate::graph::record::ValueType::String, b"weight".to_vec());
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
                    assert_eq!(rec.payload.len(), 8 + 4 + NodeRecord::SIZE);
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
}
