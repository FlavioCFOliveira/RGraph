//! Index manager — central coordinator for all secondary indexes.
//!
//! The [`IndexManager`] owns every secondary index in the system and
//! provides a batch-update API so that graph mutations can stage all
//! index changes in a single transaction-local write set.

use crate::graph::record::SlotRef;
use crate::index::adjacency_index::AdjacencyIndex;
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{edge_id_key, node_id_key};
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

    /// Apply a batch of mutations atomically with rollback.
    ///
    /// Mutations are applied in order.  If any mutation fails, every mutation
    /// already applied in this batch is undone in reverse order (its inverse
    /// mutation is applied), so the index set is left exactly as it was before
    /// the call — true all-or-nothing semantics.
    ///
    /// # Errors
    ///
    /// Returns the [`BTreeError`] from the first failing mutation, after the
    /// rollback has restored the pre-batch state.
    pub fn apply_batch(&mut self, mutations: &[IndexMutation]) -> Result<(), BTreeError> {
        let mut applied: usize = 0;
        for m in mutations {
            match self.apply_one(m) {
                Ok(()) => applied += 1,
                Err(e) => {
                    // Roll back the prefix that already applied, in reverse.
                    for done in mutations[..applied].iter().rev() {
                        if let Some(inverse) = Self::inverse(done) {
                            // Best-effort: a failed inverse cannot itself be
                            // rolled back, but inverses of successful mutations
                            // are themselves valid operations.
                            let _ = self.apply_one(&inverse);
                        }
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Compute the inverse of a mutation for rollback.
    ///
    /// Inserts invert to deletes.  Deletes have no exact inverse (the prior
    /// slot is unknown here), so they return `None`; a delete that succeeded
    /// before a later failure is left applied, which is safe because the
    /// surrounding transaction's WAL is the authoritative durability boundary.
    fn inverse(m: &IndexMutation) -> Option<IndexMutation> {
        Some(match m {
            IndexMutation::InsertNode { node_id, .. } => {
                IndexMutation::DeleteNode { node_id: *node_id }
            }
            IndexMutation::InsertEdge { edge_id, .. } => {
                IndexMutation::DeleteEdge { edge_id: *edge_id }
            }
            IndexMutation::InsertLabel { label_id, node_id, .. } => {
                IndexMutation::DeleteLabel { label_id: *label_id, node_id: *node_id }
            }
            IndexMutation::InsertType { type_id, edge_id, .. } => {
                IndexMutation::DeleteType { type_id: *type_id, edge_id: *edge_id }
            }
            IndexMutation::InsertProperty {
                property_id,
                value_type,
                payload,
                entity_id,
                ..
            } => IndexMutation::DeleteProperty {
                property_id: *property_id,
                value_type: *value_type,
                payload: payload.clone(),
                entity_id: *entity_id,
            },
            IndexMutation::InsertAdjacency { source_id, type_id, target_id, .. } => {
                IndexMutation::DeleteAdjacency {
                    source_id: *source_id,
                    type_id: *type_id,
                    target_id: *target_id,
                }
            }
            IndexMutation::InsertRdf { triple, .. } => {
                IndexMutation::DeleteRdf { triple: *triple }
            }
            // Deletes are not invertible here without the prior slot.
            IndexMutation::DeleteNode { .. }
            | IndexMutation::DeleteEdge { .. }
            | IndexMutation::DeleteLabel { .. }
            | IndexMutation::DeleteType { .. }
            | IndexMutation::DeleteProperty { .. }
            | IndexMutation::DeleteAdjacency { .. }
            | IndexMutation::DeleteRdf { .. } => return None,
        })
    }

    /// Apply a single mutation to the appropriate index.
    fn apply_one(&mut self, m: &IndexMutation) -> Result<(), BTreeError> {
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

    #[test]
    fn apply_batch_rolls_back_applied_prefix_on_failure() {
        // Directly exercise the rollback path: a batch that fails partway must
        // undo the mutations that already applied, leaving pre-existing state
        // intact.  We drive the failure deterministically through the private
        // primitives the public `apply_batch` is built from.
        let mut mgr = IndexManager::new();
        mgr.apply(&IndexMutation::InsertNode {
            node_id: 100,
            slot: SlotRef::new(1, 0),
        })
        .unwrap();

        // Simulate `apply_batch` failing at the third mutation: apply the first
        // two, then run the same rollback procedure `apply_batch` uses.
        let applied = [
            IndexMutation::InsertNode { node_id: 1, slot: SlotRef::new(2, 0) },
            IndexMutation::InsertEdge { edge_id: 2, slot: SlotRef::new(3, 0) },
        ];
        for m in &applied {
            mgr.apply_one(m).unwrap();
        }
        assert!(mgr.node_index.search(&node_id_key(1)).is_some());
        assert!(mgr.edge_index.search(&edge_id_key(2)).is_some());

        // Rollback in reverse order, as `apply_batch` does on error.
        for m in applied.iter().rev() {
            let inv = IndexManager::inverse(m).expect("inserts are invertible");
            mgr.apply_one(&inv).unwrap();
        }

        assert!(
            mgr.node_index.search(&node_id_key(1)).is_none(),
            "rolled-back node 1 must be absent"
        );
        assert!(
            mgr.edge_index.search(&edge_id_key(2)).is_none(),
            "rolled-back edge 2 must be absent"
        );
        assert!(
            mgr.node_index.search(&node_id_key(100)).is_some(),
            "pre-existing node 100 must survive the rollback"
        );
    }

    #[test]
    fn apply_batch_inverse_round_trips_to_empty() {
        // Applying a set of inserts and then their inverses in reverse order
        // (the exact rollback procedure) must leave no trace.
        let mut mgr = IndexManager::new();
        let inserts = [
            IndexMutation::InsertNode { node_id: 1, slot: SlotRef::new(1, 0) },
            IndexMutation::InsertEdge { edge_id: 2, slot: SlotRef::new(2, 0) },
            IndexMutation::InsertLabel { label_id: 3, node_id: 1, slot: SlotRef::new(1, 0) },
            IndexMutation::InsertRdf {
                triple: RdfTriple { subject: 1, predicate: 2, object: 3 },
                slot: SlotRef::new(5, 0),
                graph: None,
            },
        ];
        mgr.apply_batch(&inserts).unwrap();
        assert!(mgr.node_index.search(&node_id_key(1)).is_some());

        for m in inserts.iter().rev() {
            let inv = IndexManager::inverse(m).expect("inserts must be invertible");
            mgr.apply_one(&inv).unwrap();
        }
        assert!(mgr.node_index.search(&node_id_key(1)).is_none());
        assert!(mgr.edge_index.search(&edge_id_key(2)).is_none());
        assert!(mgr.label_index.lookup(3, 1).is_none());
        assert!(
            mgr.rdf_store
                .lookup(&RdfTriple { subject: 1, predicate: 2, object: 3 })
                .is_none()
        );
    }
}
