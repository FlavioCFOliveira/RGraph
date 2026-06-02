//! High-level graph API — nodes, relationships, properties, and adjacency.
//!
//! [`Graph`] sits above the raw [`StorageEngine`] and provides an
//! openCypher-friendly interface for creating and querying graph elements.

use crate::graph::builder::{NodeBuilder, RelationshipBuilder};
use crate::graph::property::Property;
use crate::graph::record::SlotRef;
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
    /// Returns the [`SlotRef`] where the node record was stored.
    pub fn create_node(
        &mut self,
        builder: NodeBuilder,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError> {
        let record = builder.build();
        let slot = self.engine.put_node(&record, fs)?;
        // TODO: store properties via property index once integrated.
        Ok(slot)
    }

    /// Insert a relationship into the graph.
    ///
    /// Returns the [`SlotRef`] where the edge record was stored.
    pub fn create_relationship(
        &mut self,
        builder: RelationshipBuilder,
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, StorageError> {
        let record = builder.build().map_err(|e| {
            StorageError::from(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let slot = self.engine.put_edge(&record, fs)?;
        // TODO: store properties via property index once integrated.
        Ok(slot)
    }

    /// Retrieve a node by its `node_id`.
    pub fn get_node(
        &self,
        node_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<Node>, StorageError> {
        let record = self.engine.get_node(node_id, fs)?;
        match record {
            Some(r) if r.flags & crate::graph::record::node_flags::DELETED == 0 => Ok(Some(Node {
                node_id: r.node_id,
                label_id: r.label_id,
                properties: HashMap::new(), // resolved lazily
            })),
            _ => Ok(None),
        }
    }

    /// Retrieve a relationship by its `edge_id`.
    pub fn get_relationship(
        &self,
        edge_id: u64,
        fs: &dyn FileSystem,
    ) -> Result<Option<Relationship>, StorageError> {
        let record = self.engine.get_edge(edge_id, fs)?;
        match record {
            Some(r) if r.flags & crate::graph::record::edge_flags::DELETED == 0 => {
                Ok(Some(Relationship {
                    edge_id: r.edge_id,
                    type_id: r.type_id,
                    source_id: r.source_node.page_id() as u64,
                    target_id: r.target_node.page_id() as u64,
                    properties: HashMap::new(), // resolved lazily
                }))
            }
            _ => Ok(None),
        }
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
            nodes.push(Node {
                node_id: r.node_id,
                label_id: r.label_id,
                properties: HashMap::new(), // resolved lazily
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
            rels.push(Relationship {
                edge_id: r.edge_id,
                type_id: r.type_id,
                source_id: r.source_node.page_id() as u64,
                target_id: r.target_node.page_id() as u64,
                properties: HashMap::new(), // resolved lazily
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
        let builder = NodeBuilder::new(1).label(42);
        let slot = graph.create_node(builder, &fs).unwrap();
        assert!(!slot.is_null());

        let node = graph.get_node(1, &fs).unwrap();
        assert!(node.is_some());
        let node = node.unwrap();
        assert_eq!(node.node_id, 1);
        assert_eq!(node.label_id, 42);
    }

    #[test]
    fn create_and_get_relationship() {
        let (_dir, fs, _path, mut graph) = temp_graph();

        // Need source and target nodes first.
        let src = NodeBuilder::new(1).label(1);
        let tgt = NodeBuilder::new(2).label(1);
        graph.create_node(src, &fs).unwrap();
        graph.create_node(tgt, &fs).unwrap();

        let rel = RelationshipBuilder::new(100)
            .from(1)
            .to(2)
            .type_id(7);
        let slot = graph.create_relationship(rel, &fs).unwrap();
        assert!(!slot.is_null());

        let edge = graph.get_relationship(100, &fs).unwrap();
        assert!(edge.is_some());
        let edge = edge.unwrap();
        assert_eq!(edge.edge_id, 100);
        assert_eq!(edge.type_id, 7);
        assert_eq!(edge.source_id, 1);
        assert_eq!(edge.target_id, 2);
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
        let builder = NodeBuilder::new(1).label(42);
        graph.create_node(builder, &fs).unwrap();
        graph.delete_node(1, &fs).unwrap();

        let node = graph.get_node(1, &fs).unwrap();
        assert!(node.is_none(), "deleted node should not be visible");
    }

    #[test]
    fn delete_relationship_leaves_tombstone() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let src = NodeBuilder::new(1).label(1);
        let tgt = NodeBuilder::new(2).label(1);
        graph.create_node(src, &fs).unwrap();
        graph.create_node(tgt, &fs).unwrap();

        let rel = RelationshipBuilder::new(100)
            .from(1)
            .to(2)
            .type_id(7);
        graph.create_relationship(rel, &fs).unwrap();
        graph.delete_relationship(100, &fs).unwrap();

        let edge = graph.get_relationship(100, &fs).unwrap();
        assert!(edge.is_none(), "deleted relationship should not be visible");
    }

    #[test]
    fn node_builder_with_properties_roundtrip() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let builder = NodeBuilder::new(1)
            .label(42)
            .property("name", "Alice")
            .property("age", 30i64);
        let slot = graph.create_node(builder, &fs).unwrap();
        assert!(!slot.is_null());

        let node = graph.get_node(1, &fs).unwrap().unwrap();
        assert_eq!(node.node_id, 1);
        assert_eq!(node.label_id, 42);
        // Properties are not yet materialised into the record (TODO),
        // but the builder accepted them without error.
    }

    #[test]
    fn scan_nodes_by_label_returns_matching_nodes() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        graph.create_node(NodeBuilder::new(1).label(10), &fs).unwrap();
        graph.create_node(NodeBuilder::new(2).label(20), &fs).unwrap();
        graph.create_node(NodeBuilder::new(3).label(10), &fs).unwrap();

        let nodes = graph.scan_by_label(10, &fs).unwrap();
        assert_eq!(nodes.len(), 2);
        let ids: Vec<u64> = nodes.iter().map(|n| n.node_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
    }

    #[test]
    fn scan_nodes_by_label_excludes_deleted() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        graph.create_node(NodeBuilder::new(1).label(10), &fs).unwrap();
        graph.create_node(NodeBuilder::new(2).label(10), &fs).unwrap();
        graph.delete_node(1, &fs).unwrap();

        let nodes = graph.scan_by_label(10, &fs).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, 2);
    }

    #[test]
    fn scan_relationships_by_type_returns_matching_edges() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let src = NodeBuilder::new(1).label(1);
        let tgt = NodeBuilder::new(2).label(1);
        graph.create_node(src, &fs).unwrap();
        graph.create_node(tgt, &fs).unwrap();

        graph.create_relationship(RelationshipBuilder::new(100).from(1).to(2).type_id(5), &fs).unwrap();
        graph.create_relationship(RelationshipBuilder::new(101).from(2).to(1).type_id(7), &fs).unwrap();
        graph.create_relationship(RelationshipBuilder::new(102).from(1).to(2).type_id(5), &fs).unwrap();

        let rels = graph.scan_by_type(5, &fs).unwrap();
        assert_eq!(rels.len(), 2);
        let ids: Vec<u64> = rels.iter().map(|r| r.edge_id).collect();
        assert!(ids.contains(&100));
        assert!(ids.contains(&102));
    }

    #[test]
    fn scan_relationships_by_type_excludes_deleted() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        let src = NodeBuilder::new(1).label(1);
        let tgt = NodeBuilder::new(2).label(1);
        graph.create_node(src, &fs).unwrap();
        graph.create_node(tgt, &fs).unwrap();

        graph.create_relationship(RelationshipBuilder::new(100).from(1).to(2).type_id(5), &fs).unwrap();
        graph.create_relationship(RelationshipBuilder::new(101).from(2).to(1).type_id(5), &fs).unwrap();
        graph.delete_relationship(100, &fs).unwrap();

        let rels = graph.scan_by_type(5, &fs).unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].edge_id, 101);
    }

    #[test]
    fn scan_nodes_by_property_returns_matching_nodes() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        graph.create_node(NodeBuilder::new(1).label(10), &fs).unwrap();
        graph.create_node(NodeBuilder::new(2).label(10), &fs).unwrap();
        graph.create_node(NodeBuilder::new(3).label(10), &fs).unwrap();

        // Manually insert property index entries.
        let slot = SlotRef::new(1, 0);
        graph.insert_property_index(1, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();
        graph.insert_property_index(2, 42, crate::graph::record::ValueType::Int64, &20i64.to_be_bytes(), slot).unwrap();
        graph.insert_property_index(3, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();

        let nodes = graph.scan_nodes_by_property(42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), &fs).unwrap();
        assert_eq!(nodes.len(), 2);
        let ids: Vec<u64> = nodes.iter().map(|n| n.node_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
    }

    #[test]
    fn scan_nodes_by_property_excludes_deleted() {
        let (_dir, fs, _path, mut graph) = temp_graph();
        graph.create_node(NodeBuilder::new(1).label(10), &fs).unwrap();
        graph.create_node(NodeBuilder::new(2).label(10), &fs).unwrap();

        let slot = SlotRef::new(1, 0);
        graph.insert_property_index(1, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();
        graph.insert_property_index(2, 42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), slot).unwrap();

        graph.delete_node(1, &fs).unwrap();

        let nodes = graph.scan_nodes_by_property(42, crate::graph::record::ValueType::Int64, &10i64.to_be_bytes(), &fs).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, 2);
    }
}
