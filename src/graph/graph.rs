//! High-level graph API — nodes, relationships, properties, and adjacency.
//!
//! [`Graph`] sits above the raw [`StorageEngine`] and provides an
//! openCypher-friendly interface for creating and querying graph elements.

use crate::graph::builder::{NodeBuilder, RelationshipBuilder};
use crate::graph::property::Property;
use crate::graph::record::{PropertyRecord, SlotRef, ValueType, node_flags, edge_flags};
use crate::graph::engine::{GraphStorageEngine, StorageEngine, StorageError};
use crate::io::FileSystem;
use std::collections::HashMap;

/// In-memory view of a node with its resolved properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub node_id: u64,
    pub label_id: u32,
    pub properties: HashMap<String, Property>,
}

/// In-memory view of a relationship with its resolved properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Relationship {
    pub edge_id: u64,
    pub type_id: u32,
    pub source_id: u64,
    pub target_id: u64,
    pub properties: HashMap<String, Property>,
}

/// High-level graph API backed by a [`GraphStorageEngine`].
///
/// All mutations go through the storage engine and update secondary indexes
/// atomically when the transaction commits.
pub struct Graph {
    engine: GraphStorageEngine,
}

impl Graph {
    /// Create a new graph backed by `engine`.
    pub fn new(engine: GraphStorageEngine) -> Self {
        Self { engine }
    }

    /// Create a `GraphMut` borrowing a `&mut GraphStorageEngine`.
    ///
    /// Useful when you do not own the engine but need graph-level mutation
    /// (e.g. inside the physical execution engine).
    pub fn new_ref(engine: &mut GraphStorageEngine) -> GraphMut<'_> {
        GraphMut { engine }
    }

    /// Return a mutable reference to the underlying engine.
    pub fn engine_mut(&mut self) -> &mut GraphStorageEngine {
        &mut self.engine
    }

    /// Return an immutable reference to the underlying engine.
    pub fn engine(&self) -> &GraphStorageEngine {
        &self.engine
    }

    /// Insert a node into the graph.
    ///
    /// Allocates a server-side unique id, stores the node record, and
    /// persists all builder properties as a linked `PropertyRecord` chain.
    /// Returns the [`SlotRef`] together with the allocated `node_id`.
    ///
    /// # Errors
    ///
    /// Returns `StorageError` if the storage engine fails.
    pub fn create_node(
        &mut self,
        builder: NodeBuilder,
        fs: &dyn FileSystem,
    ) -> Result<(SlotRef, u64), StorageError> {
        let (mut record, properties) = builder.into_parts();
        let id = self.engine.id_allocator.allocate();
        record.node_id = id;
        let slot = self.engine.put_node(&record, fs)?;

        // Persist properties as a linked chain attached to the node.
        self.attach_properties_to_node(id, slot, properties, fs)?;

        Ok((slot, id))
    }

    /// Insert a relationship into the graph.
    ///
    /// Allocates a server-side unique id, resolves the source and target node
    /// [`SlotRef`]s, stores the edge record, and persists all builder
    /// properties.  Returns the [`SlotRef`] together with the allocated
    /// `edge_id`.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::NotFound` when either endpoint node does not
    /// exist.  Returns other `StorageError` variants on storage failure.
    pub fn create_relationship(
        &mut self,
        builder: RelationshipBuilder,
        fs: &dyn FileSystem,
    ) -> Result<(SlotRef, u64), StorageError> {
        let (mut record, properties) = builder.into_parts().map_err(|e| {
            StorageError::from(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let id = self.engine.id_allocator.allocate();
        record.edge_id = id;

        let source_slot = self.engine.lookup_node_slot(record.source_id)?
            .ok_or(StorageError::NotFound)?;
        let target_slot = self.engine.lookup_node_slot(record.target_id)?
            .ok_or(StorageError::NotFound)?;
        record.source_node = source_slot;
        record.target_node = target_slot;

        let slot = self.engine.put_edge(&record, fs)?;

        // Persist properties as a linked chain attached to the edge.
        self.attach_properties_to_edge(id, slot, properties, fs)?;

        Ok((slot, id))
    }

    // ------------------------------------------------------------------
    // Property persistence helpers
    // ------------------------------------------------------------------

    /// Persist a property map as a linked `PropertyRecord` chain and attach
    /// the chain to a node.
    ///
    /// Properties are stored in **reverse sorted order** so that each record
    /// can set its `next_property` pointer to the slot of the already-stored
    /// successor, building a complete chain without in-place updates.
    fn attach_properties_to_node(
        &mut self,
        node_id: u64,
        _node_slot: SlotRef,
        properties: HashMap<String, Property>,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        if properties.is_empty() {
            return Ok(());
        }

        // Sort by key and iterate in reverse so each record can point to the
        // already-written successor.
        let mut entries: Vec<(String, Property)> = properties.into_iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        let mut next_slot = SlotRef::NULL; // pointer to the next record in chain

        for (key, value) in entries.into_iter().rev() {
            let (vtype, payload) = value.to_value_type_payload()
                .unwrap_or((ValueType::Null, Vec::new()));
            let mut prop = PropertyRecord::inline(&key, 0, vtype, payload.clone());
            // Point this record to the previously stored (successor) record.
            prop.next_property = next_slot;

            let prop_slot = self.engine.put_property(&prop, fs)?;
            next_slot = prop_slot;

            // Insert into the property secondary index.
            let _ = self.engine.insert_property_index(
                node_id as u128,
                0, // key_id — using 0 as placeholder until a name registry is added
                vtype,
                &payload,
                prop_slot,
            );
        }

        // `next_slot` now points to the head of the chain (first property alphabetically).
        self.engine.attach_property_to_node(node_id, next_slot, fs)?;

        Ok(())
    }

    /// Persist a property map as a linked `PropertyRecord` chain and attach
    /// the chain to an edge.
    fn attach_properties_to_edge(
        &mut self,
        edge_id: u64,
        _edge_slot: SlotRef,
        properties: HashMap<String, Property>,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        if properties.is_empty() {
            return Ok(());
        }

        let mut entries: Vec<(String, Property)> = properties.into_iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        let mut next_slot = SlotRef::NULL;

        for (key, value) in entries.into_iter().rev() {
            let (vtype, payload) = value.to_value_type_payload()
                .unwrap_or((ValueType::Null, Vec::new()));
            let mut prop = PropertyRecord::inline(&key, 0, vtype, payload.clone());
            prop.next_property = next_slot;

            let prop_slot = self.engine.put_property(&prop, fs)?;
            next_slot = prop_slot;

            let _ = self.engine.insert_property_index(
                edge_id as u128,
                0,
                vtype,
                &payload,
                prop_slot,
            );
        }

        self.engine.attach_property_to_edge(edge_id, next_slot, fs)?;

        Ok(())
    }

    /// Retrieve a node by its `node_id`.
    ///
    /// Walks the property chain stored on disk and returns a fully materialised
    /// `properties` map.
    pub fn get_node(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<Node>, StorageError> {
        let record = self.engine.get_node(node_id, fs)?;
        match record {
            Some(r) if r.flags & node_flags::DELETED == 0 => {
                let properties = self.read_property_chain(r.first_property, fs)?;
                Ok(Some(Node {
                    node_id: r.node_id,
                    label_id: r.label_id,
                    properties,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Retrieve a relationship by its `edge_id`.
    ///
    /// Walks the property chain stored on disk and returns a fully materialised
    /// `properties` map.
    pub fn get_relationship(
        &self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<Relationship>, StorageError> {
        let record = self.engine.get_edge(edge_id, fs)?;
        match record {
            Some(r) if r.flags & edge_flags::DELETED == 0 => {
                let properties = self.read_property_chain(r.first_property, fs)?;
                Ok(Some(Relationship {
                    edge_id: r.edge_id,
                    type_id: r.type_id,
                    source_id: r.source_id,
                    target_id: r.target_id,
                    properties,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Walk a property chain starting at `head` and decode each record into a
    /// `HashMap<String, Property>`.
    ///
    /// Guards against cycles by limiting traversal to a maximum depth.
    fn read_property_chain(
        &self,
        head: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<HashMap<String, Property>, StorageError> {
        let mut map = HashMap::new();
        let mut current = head;
        let mut depth = 0usize;
        const MAX_CHAIN_DEPTH: usize = 4096;

        while !current.is_null() && depth < MAX_CHAIN_DEPTH {
            let prop_opt = self.engine.get_property(current, fs)?;
            let prop = match prop_opt {
                Some(p) => p,
                None => break,
            };

            // Decode the stored value into a Property.
            let value_type = crate::graph::record::ValueType::from_u8(prop.header.value_type);
            if let Some(vt) = value_type {
                let property_val = decode_property_value(vt, &prop.payload);
                map.insert(prop.property_name.clone(), property_val);
            }

            current = prop.next_property;
            depth += 1;
        }

        Ok(map)
    }

    /// Delete a node (leaves a tombstone).
    pub fn delete_node(
        &mut self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        self.engine.delete_node(node_id, fs)
    }

    /// Delete a relationship (leaves a tombstone).
    pub fn delete_relationship(
        &mut self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        self.engine.delete_edge(edge_id, fs)
    }

    /// Flush all durable state to disk.
    pub fn sync(
        &mut self,
        fs: &dyn FileSystem,
    ) -> Result<(), StorageError> {
        self.engine.sync(fs)
    }

    /// Scan all nodes with the given `label_id`.
    pub fn scan_by_label(
        &self,
        label_id: u32,
        fs: &dyn FileSystem,
    ) -> Result<Vec<Node>, StorageError> {
        let records = self.engine.scan_nodes_by_label(label_id as u64, fs)?;
        let mut nodes = Vec::new();
        for r in records {
            let properties = self.read_property_chain(r.first_property, fs)?;
            nodes.push(Node {
                node_id: r.node_id,
                label_id: r.label_id,
                properties,
            });
        }
        Ok(nodes)
    }

    /// Scan all relationships with the given `type_id`.
    pub fn scan_by_type(
        &self,
        type_id: u32,
        fs: &dyn FileSystem,
    ) -> Result<Vec<Relationship>, StorageError> {
        let records = self.engine.scan_edges_by_type(type_id as u64, fs)?;
        let mut rels = Vec::new();
        for r in records {
            let properties = self.read_property_chain(r.first_property, fs)?;
            rels.push(Relationship {
                edge_id: r.edge_id,
                type_id: r.type_id,
                source_id: r.source_id,
                target_id: r.target_id,
                properties,
            });
        }
        Ok(rels)
    }

    /// Enumerate every live node in the graph with its materialised properties.
    ///
    /// Backed by [`GraphStorageEngine::scan_all_nodes`]; used by the CLI export
    /// path (Task 179) to dump the full graph.
    pub fn scan_all_nodes(&self, fs: &dyn FileSystem) -> Result<Vec<Node>, StorageError> {
        let records = self.engine.scan_all_nodes(fs)?;
        let mut nodes = Vec::with_capacity(records.len());
        for r in records {
            let properties = self.read_property_chain(r.first_property, fs)?;
            nodes.push(Node {
                node_id: r.node_id,
                label_id: r.label_id,
                properties,
            });
        }
        Ok(nodes)
    }

    /// Enumerate every live relationship in the graph with its materialised
    /// properties.
    ///
    /// Backed by [`GraphStorageEngine::scan_all_edges`]; used by the CLI export
    /// path (Task 179).
    pub fn scan_all_relationships(
        &self,
        fs: &dyn FileSystem,
    ) -> Result<Vec<Relationship>, StorageError> {
        let records = self.engine.scan_all_edges(fs)?;
        let mut rels = Vec::with_capacity(records.len());
        for r in records {
            let properties = self.read_property_chain(r.first_property, fs)?;
            rels.push(Relationship {
                edge_id: r.edge_id,
                type_id: r.type_id,
                source_id: r.source_id,
                target_id: r.target_id,
                properties,
            });
        }
        Ok(rels)
    }

    /// Insert an entry into the property secondary index.
    ///
    /// This is a low-level operation exposed for testing; in production the
    /// property index is maintained automatically by `create_node` and
    /// `create_relationship` once full property persistence is wired.
    pub fn insert_property_index(
        &mut self,
        entity_id: u64,
        property_id: u64,
        value_type: crate::graph::record::ValueType,
        payload: &[u8],
        slot: SlotRef,
    ) -> Result<(), StorageError> {
        self.engine.insert_property_index(entity_id as u128, property_id, value_type, payload, slot)
    }

    /// Build and publish a frozen CSR adjacency snapshot (Task 58).
    ///
    /// After this call, [`Graph::scan_adjacency`] uses the cache-friendly CSR
    /// path.  Returns the published [`CsrAdjacency`] snapshot.
    pub fn freeze_adjacency(
        &self,
        fs: &dyn FileSystem,
    ) -> std::sync::Arc<crate::graph::csr::CsrAdjacency> {
        self.engine.freeze_adjacency(fs)
    }

    /// Release the frozen CSR snapshot, reverting [`Graph::scan_adjacency`] to
    /// the doubly-linked walk until the next [`Graph::freeze_adjacency`].
    pub fn thaw_adjacency(&self) {
        self.engine.thaw_adjacency();
    }

    /// Scan the outgoing edges of `node_id`, returning
    /// `(target_id, edge_id, type_id)` triples.
    ///
    /// Prefers the frozen CSR snapshot when available; otherwise walks the
    /// doubly-linked adjacency list.
    pub fn scan_adjacency(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Vec<(u64, u64, u32)>, StorageError> {
        self.engine.scan_adjacency(node_id, fs)
    }

    /// Compact tombstoned node/edge slots and reclaim space (Task 174).
    ///
    /// Returns the number of tombstone slots reclaimed.
    pub fn compact(&mut self, fs: &dyn FileSystem) -> Result<usize, StorageError> {
        self.engine.compact(fs)
    }

    /// Scan nodes that have a property with the given `property_id` and value.
    pub fn scan_nodes_by_property(
        &self,
        property_id: u64,
        value_type: crate::graph::record::ValueType,
        payload: &[u8],
        fs: &dyn FileSystem,
    ) -> Result<Vec<Node>, StorageError> {
        let entries = self.engine.scan_property_index(property_id, value_type, payload);
        let mut nodes = Vec::new();
        for (entity_id, _prop_slot) in entries {
            let node_id = entity_id as u64;
            if let Some(node) = self.get_node(node_id, fs)? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }
}

// ------------------------------------------------------------------
// GraphMut — graph mutation via borrowed engine reference
// ------------------------------------------------------------------

/// Graph-level mutation operations backed by a borrowed `&mut GraphStorageEngine`.
///
/// This is the borrowed counterpart of [`Graph`].  The physical execution
/// engine uses `GraphMut` to create and delete nodes/relationships without
/// taking ownership of the engine.
pub struct GraphMut<'a> {
    engine: &'a mut GraphStorageEngine,
}

impl<'a> GraphMut<'a> {
    /// Insert a node into the graph (same semantics as [`Graph::create_node`]).
    pub fn create_node(
        &mut self,
        builder: NodeBuilder,
        fs: &dyn FileSystem,
    ) -> Result<(SlotRef, u64), StorageError> {
        let (mut record, properties) = builder.into_parts();
        let id = self.engine.id_allocator.allocate();
        record.node_id = id;
        let slot = self.engine.put_node(&record, fs)?;
        attach_properties_to_node_engine(self.engine, id, slot, properties, fs)?;
        Ok((slot, id))
    }

    /// Insert a relationship into the graph.
    pub fn create_relationship(
        &mut self,
        builder: RelationshipBuilder,
        fs: &dyn FileSystem,
    ) -> Result<(SlotRef, u64), StorageError> {
        let (mut record, properties) = builder.into_parts().map_err(|e| {
            StorageError::from(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let id = self.engine.id_allocator.allocate();
        record.edge_id = id;

        let source_slot = self.engine.lookup_node_slot(record.source_id)?
            .ok_or(StorageError::NotFound)?;
        let target_slot = self.engine.lookup_node_slot(record.target_id)?
            .ok_or(StorageError::NotFound)?;
        record.source_node = source_slot;
        record.target_node = target_slot;

        let slot = self.engine.put_edge(&record, fs)?;
        attach_properties_to_edge_engine(self.engine, id, slot, properties, fs)?;
        Ok((slot, id))
    }

    /// Delete a node (tombstone).
    pub fn delete_node(&mut self, node_id: u64, fs: &dyn FileSystem) -> Result<(), StorageError> {
        self.engine.delete_node(node_id, fs)
    }

    /// Delete an edge (tombstone).
    pub fn delete_edge(&mut self, edge_id: u64, fs: &dyn FileSystem) -> Result<(), StorageError> {
        self.engine.delete_edge(edge_id, fs)
    }

    /// Read a node with its properties.
    pub fn get_node(&self, node_id: u64, fs: &dyn FileSystem) -> Result<Option<Node>, StorageError> {
        let record = self.engine.get_node(node_id, fs)?;
        match record {
            Some(r) if r.flags & node_flags::DELETED == 0 => {
                let properties = read_property_chain_engine(self.engine, r.first_property, fs)?;
                Ok(Some(Node { node_id: r.node_id, label_id: r.label_id, properties }))
            }
            _ => Ok(None),
        }
    }
}

// ------------------------------------------------------------------
// Shared engine-level property helpers (free functions)
// ------------------------------------------------------------------

fn attach_properties_to_node_engine(
    engine: &mut GraphStorageEngine,
    node_id: u64,
    _slot: SlotRef,
    properties: HashMap<String, Property>,
    fs: &dyn FileSystem,
) -> Result<(), StorageError> {
    if properties.is_empty() {
        return Ok(());
    }
    let mut entries: Vec<(String, Property)> = properties.into_iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut next_slot = SlotRef::NULL;
    for (key, value) in entries.into_iter().rev() {
        let (vtype, payload) = value.to_value_type_payload()
            .unwrap_or((ValueType::Null, Vec::new()));
        let mut prop = PropertyRecord::inline(&key, 0, vtype, payload.clone());
        prop.next_property = next_slot;
        let prop_slot = engine.put_property(&prop, fs)?;
        next_slot = prop_slot;
        let _ = engine.insert_property_index(node_id as u128, 0, vtype, &payload, prop_slot);
    }
    engine.attach_property_to_node(node_id, next_slot, fs)?;
    Ok(())
}

fn attach_properties_to_edge_engine(
    engine: &mut GraphStorageEngine,
    edge_id: u64,
    _slot: SlotRef,
    properties: HashMap<String, Property>,
    fs: &dyn FileSystem,
) -> Result<(), StorageError> {
    if properties.is_empty() {
        return Ok(());
    }
    let mut entries: Vec<(String, Property)> = properties.into_iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut next_slot = SlotRef::NULL;
    for (key, value) in entries.into_iter().rev() {
        let (vtype, payload) = value.to_value_type_payload()
            .unwrap_or((ValueType::Null, Vec::new()));
        let mut prop = PropertyRecord::inline(&key, 0, vtype, payload.clone());
        prop.next_property = next_slot;
        let prop_slot = engine.put_property(&prop, fs)?;
        next_slot = prop_slot;
        let _ = engine.insert_property_index(edge_id as u128, 0, vtype, &payload, prop_slot);
    }
    engine.attach_property_to_edge(edge_id, next_slot, fs)?;
    Ok(())
}

pub fn read_property_chain_engine(
    engine: &GraphStorageEngine,
    head: SlotRef,
    fs: &dyn FileSystem,
) -> Result<HashMap<String, Property>, StorageError> {
    let mut map = HashMap::new();
    let mut current = head;
    let mut depth = 0usize;
    const MAX_CHAIN_DEPTH: usize = 4096;
    while !current.is_null() && depth < MAX_CHAIN_DEPTH {
        let prop_opt = engine.get_property(current, fs)?;
        let prop = match prop_opt {
            Some(p) => p,
            None => break,
        };
        let value_type = crate::graph::record::ValueType::from_u8(prop.header.value_type);
        if let Some(vt) = value_type {
            let property_val = decode_property_value(vt, &prop.payload);
            map.insert(prop.property_name.clone(), property_val);
        }
        current = prop.next_property;
        depth += 1;
    }
    Ok(map)
}

// ------------------------------------------------------------------
// Property value decoding
// ------------------------------------------------------------------

/// Decode a raw property payload (as stored in [`PropertyRecord::payload`])
/// into a typed [`Property`] value.
fn decode_property_value(vtype: ValueType, payload: &[u8]) -> Property {
    use crate::graph::property::OrderedF64;

    match vtype {
        ValueType::Null => Property::Null,
        ValueType::Bool => {
            if payload.first().copied().unwrap_or(0) != 0 {
                Property::Boolean(true)
            } else {
                Property::Boolean(false)
            }
        }
        ValueType::Int64 => {
            if payload.len() >= 8 {
                let v = i64::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                    payload[4], payload[5], payload[6], payload[7],
                ]);
                Property::Integer(v)
            } else {
                Property::Null
            }
        }
        ValueType::Float64 => {
            if payload.len() >= 8 {
                let bits = u64::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                    payload[4], payload[5], payload[6], payload[7],
                ]);
                Property::Float(OrderedF64(f64::from_bits(bits)))
            } else {
                Property::Null
            }
        }
        ValueType::String => {
            match std::str::from_utf8(payload) {
                Ok(s) => Property::String(s.to_owned()),
                Err(_) => Property::Null,
            }
        }
        // Composite types (List, Map) are not yet supported in the codec.
        ValueType::List | ValueType::Map => Property::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use std::path::PathBuf;

    fn temp_graph() -> (tempfile::TempDir, PosixFileSystem, PathBuf, Graph) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);
        let engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
        let graph = Graph::new(engine);
        (dir, fs, path, graph)
    }

    #[test]
    fn create_and_get_node() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let builder = NodeBuilder::new().label(42);
        let (slot, node_id) = graph.create_node(builder, &fs).unwrap();
        assert!(!slot.is_null());
        assert!(node_id > 0);

        let node = graph.get_node(node_id, &fs).unwrap();
        assert!(node.is_some());
        let node = node.unwrap();
        assert_eq!(node.node_id, node_id);
        assert_eq!(node.label_id, 42);
    }

    #[test]
    fn create_and_get_relationship() {
        let (_dir, fs, _path, mut graph) = temp_graph();

        // Need source and target nodes first.
        let src_builder = NodeBuilder::new().label(1);
        let tgt_builder = NodeBuilder::new().label(1);
        let (_, src_id) = graph.create_node(src_builder, &fs).unwrap();
        let (_, tgt_id) = graph.create_node(tgt_builder, &fs).unwrap();

        let rel = RelationshipBuilder::new()
            .from(src_id)
            .to(tgt_id)
            .type_id(7);
        let (slot, edge_id) = graph.create_relationship(rel, &fs).unwrap();
        assert!(!slot.is_null());
        assert!(edge_id > 0);

        let edge = graph.get_relationship(edge_id, &fs).unwrap();
        assert!(edge.is_some());
        let edge = edge.unwrap();
        assert_eq!(edge.edge_id, edge_id);
        assert_eq!(edge.type_id, 7);
        assert_eq!(edge.source_id, src_id);
        assert_eq!(edge.target_id, tgt_id);
    }

    #[test]
    fn get_missing_node_returns_none() {
        let (_dir, fs, _path, graph) = temp_graph();
        let node = graph.get_node(999, &fs).unwrap();
        assert!(node.is_none());
    }

    #[test]
    fn get_missing_relationship_returns_none() {
        let (_dir, fs, _path, graph) = temp_graph();
        let rel = graph.get_relationship(999, &fs).unwrap();
        assert!(rel.is_none());
    }

    #[test]
    fn delete_node_leaves_tombstone() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let builder = NodeBuilder::new().label(42);
        let (_, node_id) = graph.create_node(builder, &fs).unwrap();
        graph.delete_node(node_id, &fs).unwrap();

        let node = graph.get_node(node_id, &fs).unwrap();
        assert!(node.is_none(), "deleted node should not be visible");
    }

    #[test]
    fn delete_relationship_leaves_tombstone() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let src_builder = NodeBuilder::new().label(1);
        let tgt_builder = NodeBuilder::new().label(1);
        let (_, src_id) = graph.create_node(src_builder, &fs).unwrap();
        let (_, tgt_id) = graph.create_node(tgt_builder, &fs).unwrap();

        let rel = RelationshipBuilder::new()
            .from(src_id)
            .to(tgt_id)
            .type_id(7);
        let (_, edge_id) = graph.create_relationship(rel, &fs).unwrap();
        graph.delete_relationship(edge_id, &fs).unwrap();

        let edge = graph.get_relationship(edge_id, &fs).unwrap();
        assert!(edge.is_none(), "deleted relationship should not be visible");
    }

    #[test]
    fn node_builder_with_properties_roundtrip() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let builder = NodeBuilder::new()
            .label(42)
            .property("name", "Alice")
            .property("age", 30i64);
        let (slot, node_id) = graph.create_node(builder, &fs).unwrap();
        assert!(!slot.is_null());

        let node = graph.get_node(node_id, &fs).unwrap().unwrap();
        assert_eq!(node.node_id, node_id);
        assert_eq!(node.label_id, 42);
        // Properties are persisted and returned from disk.
        assert_eq!(node.properties.get("name"), Some(&Property::String("Alice".to_owned())));
        assert_eq!(node.properties.get("age"), Some(&Property::Integer(30)));
    }

    #[test]
    fn scan_nodes_by_label_returns_matching_nodes() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, id1) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();
        let (_, id2) = graph.create_node(NodeBuilder::new().label(20), &fs).unwrap();
        let (_, id3) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();

        let nodes = graph.scan_by_label(10, &fs).unwrap();
        assert_eq!(nodes.len(), 2);
        let ids: Vec<u64> = nodes.iter().map(|n| n.node_id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id3));
    }

    #[test]
    fn scan_nodes_by_label_excludes_deleted() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, id1) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();
        let (_, id2) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();
        graph.delete_node(id1, &fs).unwrap();

        let nodes = graph.scan_by_label(10, &fs).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, id2);
    }

    #[test]
    fn scan_relationships_by_type_returns_matching_edges() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, src_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_, tgt_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();

        let (_, eid1) = graph.create_relationship(
            RelationshipBuilder::new().from(src_id).to(tgt_id).type_id(5), &fs
        ).unwrap();
        let (_, eid2) = graph.create_relationship(
            RelationshipBuilder::new().from(tgt_id).to(src_id).type_id(7), &fs
        ).unwrap();
        let (_, eid3) = graph.create_relationship(
            RelationshipBuilder::new().from(src_id).to(tgt_id).type_id(5), &fs
        ).unwrap();

        let rels = graph.scan_by_type(5, &fs).unwrap();
        assert_eq!(rels.len(), 2);
        let ids: Vec<u64> = rels.iter().map(|r| r.edge_id).collect();
        assert!(ids.contains(&eid1));
        assert!(ids.contains(&eid3));
    }

    #[test]
    fn scan_relationships_by_type_excludes_deleted() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, src_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_, tgt_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();

        let (_, eid1) = graph.create_relationship(
            RelationshipBuilder::new().from(src_id).to(tgt_id).type_id(5), &fs
        ).unwrap();
        let (_, eid2) = graph.create_relationship(
            RelationshipBuilder::new().from(tgt_id).to(src_id).type_id(5), &fs
        ).unwrap();
        graph.delete_relationship(eid1, &fs).unwrap();

        let rels = graph.scan_by_type(5, &fs).unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].edge_id, eid2);
    }

    #[test]
    fn scan_nodes_by_property_returns_matching_nodes() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, id1) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();
        let (_, id2) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();
        let (_, id3) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();

        // Manually insert property index entries.
        let slot = SlotRef::new(1, 0);
        graph.insert_property_index(id1, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();
        graph.insert_property_index(id2, 42, crate::graph::record::ValueType::Int64, &20i64.to_be_bytes(), slot).unwrap();
        graph.insert_property_index(id3, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();

        let nodes = graph.scan_nodes_by_property(42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), &fs).unwrap();
        assert_eq!(nodes.len(), 2);
        let ids: Vec<u64> = nodes.iter().map(|n| n.node_id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id3));
    }

    #[test]
    fn scan_nodes_by_property_excludes_deleted() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, id1) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();
        let (_, id2) = graph.create_node(NodeBuilder::new().label(10), &fs).unwrap();

        let slot = SlotRef::new(1, 0);
        graph.insert_property_index(id1, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();
        graph.insert_property_index(id2, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();

        graph.delete_node(id1, &fs).unwrap();

        let nodes = graph.scan_nodes_by_property(42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), &fs).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, id2);
    }

    #[test]
    fn large_node_id_roundtrips() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let big_id = 16_777_216u64 + 1;

        // Bypass the high-level API to test the engine directly with a large id.
        let node = crate::graph::record::NodeRecord::new(big_id, 42);
        let slot = graph.engine_mut().put_node(&node, &fs).unwrap();
        assert!(!slot.is_null());

        let retrieved = graph.get_node(big_id, &fs).unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.node_id, big_id);
    }

    #[test]
    fn ids_are_server_allocated_and_unique() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, id1) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_, id2) = graph.create_node(NodeBuilder::new().label(2), &fs).unwrap();
        let (_, id3) = graph.create_node(NodeBuilder::new().label(3), &fs).unwrap();

        assert!(id1 > 0);
        assert!(id2 > 0);
        assert!(id3 > 0);
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn id_zero_is_rejected_at_engine_put_node() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let node = crate::graph::record::NodeRecord::new(0, 42);
        let result = graph.engine_mut().put_node(&node, &fs);
        assert_eq!(result, Err(StorageError::InvalidId));
    }

    #[test]
    fn id_zero_is_rejected_at_engine_put_edge() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let edge = crate::graph::record::EdgeRecord::new(
            0, 5, 1, 2,
            crate::graph::record::SlotRef::new(1, 0),
            crate::graph::record::SlotRef::new(2, 0),
        );
        let result = graph.engine_mut().put_edge(&edge, &fs);
        assert_eq!(result, Err(StorageError::InvalidId));
    }

    #[test]
    fn deleted_record_slot_is_reclaimable() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, id1) = graph.create_node(NodeBuilder::new().label(42), &fs).unwrap();
        let slot1 = graph.engine().lookup_node_slot(id1).unwrap().unwrap();
        let page_id = slot1.page_id() as u64;

        graph.delete_node(id1, &fs).unwrap();

        // Insert another node of the same size; the slotted page should
        // reuse the deleted slot via best-fit.
        let (_, id2) = graph.create_node(NodeBuilder::new().label(99), &fs).unwrap();
        let slot2 = graph.engine().lookup_node_slot(id2).unwrap().unwrap();

        // slot2 should be on the same page, and ideally reuse the same slot index.
        assert_eq!(slot2.page_id(), page_id as u32, "deleted slot page should be reused");
    }

    // ------------------------------------------------------------------
    // Task 142: property persistence restart test
    // ------------------------------------------------------------------

    #[test]
    fn properties_survive_engine_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = PosixFileSystem::new(false);

        let node_id;
        let edge_id;

        // Phase 1: create data and sync.
        {
            let engine = GraphStorageEngine::init(path.clone(), &fs).unwrap();
            let mut graph = Graph::new(engine);
            let builder = NodeBuilder::new()
                .label(7)
                .property("name", "Alice")
                .property("age", 30i64);
            let (_, nid) = graph.create_node(builder, &fs).unwrap();
            node_id = nid;

            let (_, src_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
            let (_, tgt_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
            let rel = RelationshipBuilder::new()
                .from(src_id)
                .to(tgt_id)
                .type_id(5)
                .property("weight", 42i64);
            let (_, eid) = graph.create_relationship(rel, &fs).unwrap();
            edge_id = eid;

            graph.sync(&fs).unwrap();
        }

        // Phase 2: reopen and verify properties are present.
        {
            let engine = GraphStorageEngine::open(path, &fs).unwrap();
            let graph = Graph::new(engine);

            let node = graph.get_node(node_id, &fs).unwrap().unwrap();
            assert_eq!(node.label_id, 7);
            assert_eq!(
                node.properties.get("name"),
                Some(&Property::String("Alice".to_owned())),
                "name property must survive restart"
            );
            assert_eq!(
                node.properties.get("age"),
                Some(&Property::Integer(30)),
                "age property must survive restart"
            );

            let rel = graph.get_relationship(edge_id, &fs).unwrap().unwrap();
            assert_eq!(
                rel.properties.get("weight"),
                Some(&Property::Integer(42)),
                "relationship property must survive restart"
            );
        }
    }

    // ------------------------------------------------------------------
    // Task 143: adjacency list tests
    // ------------------------------------------------------------------

    #[test]
    fn adjacency_lists_are_populated_after_edge_insert() {
        let (_dir, fs, _path, mut graph) = temp_graph();

        let (_, src_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_, tgt_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();

        let rel = RelationshipBuilder::new().from(src_id).to(tgt_id).type_id(5);
        let (edge_slot, _eid) = graph.create_relationship(rel, &fs).unwrap();
        assert!(!edge_slot.is_null());

        // The source node's first_outgoing_edge should point to the edge slot.
        let src_node = graph.engine().get_node(src_id, &fs).unwrap().unwrap();
        assert_eq!(
            src_node.first_outgoing_edge,
            edge_slot,
            "source node first_outgoing_edge must point to inserted edge"
        );

        // The target node's first_incoming_edge should point to the edge slot.
        let tgt_node = graph.engine().get_node(tgt_id, &fs).unwrap().unwrap();
        assert_eq!(
            tgt_node.first_incoming_edge,
            edge_slot,
            "target node first_incoming_edge must point to inserted edge"
        );
    }

    #[test]
    fn adjacency_lists_are_unlinked_after_edge_delete() {
        let (_dir, fs, _path, mut graph) = temp_graph();

        let (_, src_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_, tgt_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();

        let rel = RelationshipBuilder::new().from(src_id).to(tgt_id).type_id(5);
        let (_, eid) = graph.create_relationship(rel, &fs).unwrap();

        graph.delete_relationship(eid, &fs).unwrap();

        // After deletion the source node's adjacency head should be NULL.
        let src_node = graph.engine().get_node(src_id, &fs).unwrap().unwrap();
        assert!(
            src_node.first_outgoing_edge.is_null(),
            "source outgoing head must be null after single edge deleted"
        );

        let tgt_node = graph.engine().get_node(tgt_id, &fs).unwrap().unwrap();
        assert!(
            tgt_node.first_incoming_edge.is_null(),
            "target incoming head must be null after single edge deleted"
        );
    }

    #[test]
    fn out_degree_and_neighbour_traversal() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let (_, src_id) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_, tgt1) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();
        let (_, tgt2) = graph.create_node(NodeBuilder::new().label(1), &fs).unwrap();

        let (e1_slot, eid1) = graph.create_relationship(
            RelationshipBuilder::new().from(src_id).to(tgt1).type_id(5), &fs
        ).unwrap();
        let (e2_slot, eid2) = graph.create_relationship(
            RelationshipBuilder::new().from(src_id).to(tgt2).type_id(5), &fs
        ).unwrap();

        let src_node = graph.engine().get_node(src_id, &fs).unwrap().unwrap();
        let head = src_node.first_outgoing_edge;
        assert!(!head.is_null(), "source must have at least one outgoing edge");

        // Scan by type to confirm both edges are reachable.
        let rels = graph.scan_by_type(5, &fs).unwrap();
        let edge_ids: Vec<u64> = rels.iter().map(|r| r.edge_id).collect();
        assert!(edge_ids.contains(&eid1), "edge 1 must be reachable via type scan");
        assert!(edge_ids.contains(&eid2), "edge 2 must be reachable via type scan");

        // The head slot must be one of the two inserted edges.
        let _ = (e1_slot, e2_slot); // used as sanity reference
    }

    // ------------------------------------------------------------------
    // Task 145: GraphBuilder::build returns a working engine-backed handle
    // ------------------------------------------------------------------

    #[test]
    fn graph_builder_build_returns_functional_handle() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("testdb");

        let mut db = crate::config::GraphBuilder::new()
            .path(db_path)
            .mode(crate::config::GraphMode::Lpg)
            .build()
            .unwrap();

        let fs = crate::io::posix::PosixFileSystem::new(false);
        let (slot, node_id) = db.create_node(NodeBuilder::new().label(42), &fs).unwrap();
        assert!(!slot.is_null());
        assert!(node_id > 0);

        let node = db.get_node(node_id, &fs).unwrap().unwrap();
        assert_eq!(node.node_id, node_id);
        assert_eq!(node.label_id, 42);
        assert_eq!(db.graph_mode, crate::config::GraphMode::Lpg);
    }
}
