//! Index manager — central coordinator for all secondary indexes.
//!
//! The [`IndexManager`] owns every secondary index in the system and
//! provides a batch-update API so that graph mutations can stage all
//! index changes in a single transaction-local write set.

use crate::graph::record::SlotRef;
use crate::index::adjacency_index::AdjacencyIndex;
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{edge_id_key, label_index_key, node_id_key, type_index_key};
use crate::index::label::LabelIndex;
use crate::graph::record::ValueType;
use crate::index::property::PropertyIndex;
use crate::index::rdf_store::{RdfStore, RdfTriple};
use crate::index::type_index::TypeIndex;

/// A single staged mutation in a transaction-local write set.
#[derive(Debug, Clone)]
pub enum IndexMutation {
    /// Insert a node into the node id index.
    InsertNode { node_id: u64, slot: SlotRef },
    /// Delete a node from the node id index.
    DeleteNode { node_id: u64 },
    /// Insert an edge into the edge id index.
    InsertEdge { edge_id: u64, slot: SlotRef },
    /// Delete an edge from the edge id index.
    DeleteEdge { edge_id: u64 },
    /// Insert a label mapping.
    InsertLabel { label_id: u64, node_id: u128, slot: SlotRef },
    /// Delete a label mapping.
    DeleteLabel { label_id: u64, node_id: u128 },
    /// Insert a type mapping.
    InsertType { type_id: u64, edge_id: u128, slot: SlotRef },
    /// Delete a type mapping.
    DeleteType { type_id: u64, edge_id: u128 },
    /// Insert a property mapping.
    InsertProperty {
        property_id: u64,
        value_type: ValueType,
        payload: Vec<u8>,
        entity_id: u128,
        slot: SlotRef,
    },
    /// Delete a property mapping.
    DeleteProperty {
        property_id: u64,
        value_type: ValueType,
        payload: Vec<u8>,
        entity_id: u128,
    },
    /// Insert an adjacency entry.
    InsertAdjacency {
        source_id: u128,
        type_id: u64,
        target_id: u128,
        slot: SlotRef,
    },
    /// Delete an adjacency entry.
    DeleteAdjacency {
        source_id: u128,
        type_id: u64,
        target_id: u128,
    },
    /// Insert an RDF triple.
    InsertRdf {
        triple: RdfTriple,
        slot: SlotRef,
        graph: Option<u64>,
    },
    /// Delete an RDF triple.
    DeleteRdf { triple: RdfTriple },
}

/// Central coordinator that owns all secondary indexes.
#[derive(Debug)]
pub struct IndexManager {
    pub node_index: BPlusTree,
    pub edge_index: BPlusTree,
    pub label_index: LabelIndex,
    pub type_index: TypeIndex,
    pub property_index: PropertyIndex,
    pub adjacency_index: AdjacencyIndex,
    pub rdf_store: RdfStore,
}

impl Default for IndexManager {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexManager {
    /// Create a new index manager with empty indexes.
    pub fn new() -> Self {
        let config = BPlusTreeConfig::default();
        Self {
            node_index: BPlusTree::new(config.clone()),
            edge_index: BPlusTree::new(config.clone()),
            label_index: LabelIndex::new(),
            type_index: TypeIndex::new(),
            property_index: PropertyIndex::new(),
            adjacency_index: AdjacencyIndex::new(),
            rdf_store: RdfStore::new(),
        }
    }

    /// Apply a batch of mutations atomically.
    ///
    /// All mutations are applied in order.  If any mutation fails,
    /// the error is returned immediately and the caller is responsible
    /// for rolling back already-applied changes (or aborting the
    /// transaction).
    pub fn apply_batch(&mut self,
        mutations: &[IndexMutation],
    ) -> Result<(), BTreeError> {
        for m in mutations {
            match m {
                IndexMutation::InsertNode { node_id, slot } => {
                    let key = node_id_key(*node_id as u128);
                    self.node_index.insert(&key, &slot.raw.to_be_bytes())?;
                }
                IndexMutation::DeleteNode { node_id } => {
                    let key = node_id_key(*node_id as u128);
                    self.node_index.delete(&key)?;
                }
                IndexMutation::InsertEdge { edge_id, slot } => {
                    let key = edge_id_key(*edge_id as u128);
                    self.edge_index.insert(&key, &slot.raw.to_be_bytes())?;
                }
                IndexMutation::DeleteEdge { edge_id } => {
                    let key = edge_id_key(*edge_id as u128);
                    self.edge_index.delete(&key)?;
                }
                IndexMutation::InsertLabel { label_id, node_id, slot } => {
                    self.label_index.insert(*label_id, *node_id, *slot)?;
                }
                IndexMutation::DeleteLabel { label_id, node_id } => {
                    self.label_index.delete(*label_id, *node_id)?;
                }
                IndexMutation::InsertType { type_id, edge_id, slot } => {
                    self.type_index.insert(*type_id, *edge_id, *slot)?;
                }
                IndexMutation::DeleteType { type_id, edge_id } => {
                    self.type_index.delete(*type_id, *edge_id)?;
                }
                IndexMutation::InsertProperty {
                    property_id,
                    value_type,
                    payload,
                    entity_id,
                    slot,
                } => {
                    self.property_index
                        .insert(*property_id, *value_type, payload, *entity_id, *slot)?;
                }
                IndexMutation::DeleteProperty {
                    property_id,
                    value_type,
                    payload,
                    entity_id,
                } => {
                    self.property_index
                        .delete(*property_id, *value_type, payload, *entity_id)?;
                }
                IndexMutation::InsertAdjacency {
                    source_id,
                    type_id,
                    target_id,
                    slot,
                } => {
                    self.adjacency_index
                        .insert(*source_id, *type_id, *target_id, *slot)?;
                }
                IndexMutation::DeleteAdjacency {
                    source_id,
                    type_id,
                    target_id,
                } => {
                    self.adjacency_index
                        .delete(*source_id, *type_id, *target_id)?;
                }
                IndexMutation::InsertRdf { triple, slot, graph } => {
                    self.rdf_store.insert(triple, *slot, *graph)?;
                }
                IndexMutation::DeleteRdf { triple } => {
                    self.rdf_store.delete(triple)?;
                }
            }
        }
        Ok(())
    }

    /// Convenience: apply a single mutation.
    pub fn apply(&mut self, mutation: &IndexMutation) -> Result<(), BTreeError> {
        self.apply_batch(std::slice::from_ref(mutation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_insert_and_lookup() {
        let mut mgr = IndexManager::new();
        let mutations = [
            IndexMutation::InsertNode {
                node_id: 1,
                slot: SlotRef::new(10, 5),
            },
            IndexMutation::InsertLabel {
                label_id: 7,
                node_id: 1,
                slot: SlotRef::new(10, 5),
            },
            IndexMutation::InsertProperty {
                property_id: 3,
                value_type: ValueType::Int64,
                payload: 42i64.to_be_bytes().to_vec(),
                entity_id: 1,
                slot: SlotRef::new(10, 6),
            },
        ];
        mgr.apply_batch(&mutations).unwrap();

        assert!(mgr.node_index.search(&node_id_key(1)).is_some());
        assert!(mgr.label_index.lookup(7, 1).is_some());
        assert!(mgr.property_index.lookup(3, ValueType::Int64, &42i64.to_be_bytes(), 1).is_some());
    }

    #[test]
    fn batch_delete_removes_entries() {
        let mut mgr = IndexManager::new();
        mgr.apply(&IndexMutation::InsertNode {
            node_id: 2,
            slot: SlotRef::new(20, 0),
        })
        .unwrap();
        mgr.apply(&IndexMutation::DeleteNode { node_id: 2 }).unwrap();
        assert!(mgr.node_index.search(&node_id_key(2)).is_none());
    }

    #[test]
    fn batch_rdf_roundtrip() {
        let mut mgr = IndexManager::new();
        let triple = RdfTriple {
            subject: 1,
            predicate: 10,
            object: 100,
        };
        mgr.apply(&IndexMutation::InsertRdf {
            triple,
            slot: SlotRef::new(5, 3),
            graph: Some(7),
        })
        .unwrap();
        assert!(mgr.rdf_store.lookup(&triple).is_some());

        mgr.apply(&IndexMutation::DeleteRdf { triple }).unwrap();
        assert!(mgr.rdf_store.lookup(&triple).is_none());
    }

    #[test]
    fn batch_adjacency_roundtrip() {
        let mut mgr = IndexManager::new();
        mgr.apply(&IndexMutation::InsertAdjacency {
            source_id: 1,
            type_id: 10,
            target_id: 100,
            slot: SlotRef::new(5, 3),
        })
        .unwrap();
        assert!(mgr.adjacency_index.lookup(1, 10, 100).is_some());

        mgr.apply(&IndexMutation::DeleteAdjacency {
            source_id: 1,
            type_id: 10,
            target_id: 100,
        })
        .unwrap();
        assert!(mgr.adjacency_index.lookup(1, 10, 100).is_none());
    }
}
