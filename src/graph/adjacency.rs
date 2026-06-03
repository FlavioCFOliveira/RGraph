//! Doubly-linked adjacency list maintenance.
//!
//! Adjacency lists are stored implicitly through the `prev`/`next` slots
//! inside each [`EdgeRecord`].  This module provides the operations needed
//! to insert a new edge at the head of a node's adjacency list and to
//! remove an edge in O(1) time by patching neighbour pointers.

use crate::graph::record::{EdgeRecord, SlotRef};

/// Result of an adjacency operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjacencyError {
    /// The edge record is already linked to a different list.
    AlreadyLinked,
    /// The requested slot does not exist or is a tombstone.
    InvalidSlot,
    /// The edge has no adjacency pointers (null source or target).
    NullNode,
}

impl std::fmt::Display for AdjacencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdjacencyError::AlreadyLinked => write!(f, "edge already linked to an adjacency list"),
            AdjacencyError::InvalidSlot => write!(f, "invalid slot reference"),
            AdjacencyError::NullNode => write!(f, "edge has no source or target node"),
        }
    }
}

impl std::error::Error for AdjacencyError {}

/// Mutable view of an edge record's adjacency fields.
///
/// This trait abstracts over however the caller stores edge records
/// (e.g. a [`SlottedPage`](crate::storage::page::SlottedPage),
/// an in-memory map, or a buffer-pool frame).
pub trait EdgeRecordView {
    /// Read the edge record at `slot`.
    fn read(&self, slot: SlotRef) -> Option<EdgeRecord>;
    /// Write the edge record at `slot`.
    fn write(&mut self, slot: SlotRef, record: &EdgeRecord);
}

/// Link `new_edge` at the head of the source node's outgoing adjacency list.
///
/// `head` is the current first outgoing edge of the source node.
/// Returns the new head slot (which is `new_edge`).
pub fn link_source_head<V: EdgeRecordView>(
    view: &mut V,
    new_edge: SlotRef,
    head: SlotRef,
) -> Result<SlotRef, AdjacencyError> {
    let mut rec = view.read(new_edge).ok_or(AdjacencyError::InvalidSlot)?;
    if rec.source_node.is_null() {
        return Err(AdjacencyError::NullNode);
    }
    if !rec.next_source_edge.is_null() || !rec.prev_source_edge.is_null() {
        return Err(AdjacencyError::AlreadyLinked);
    }

    // new_edge -> next = head, prev = null
    rec.next_source_edge = head;
    rec.prev_source_edge = SlotRef::NULL;
    view.write(new_edge, &rec);

    if !head.is_null() {
        let mut head_rec = view.read(head).ok_or(AdjacencyError::InvalidSlot)?;
        head_rec.prev_source_edge = new_edge;
        view.write(head, &head_rec);
    }

    Ok(new_edge)
}

/// Link `new_edge` at the head of the target node's incoming adjacency list.
///
/// `head` is the current first incoming edge of the target node.
/// Returns the new head slot (which is `new_edge`).
pub fn link_target_head<V: EdgeRecordView>(
    view: &mut V,
    new_edge: SlotRef,
    head: SlotRef,
) -> Result<SlotRef, AdjacencyError> {
    let mut rec = view.read(new_edge).ok_or(AdjacencyError::InvalidSlot)?;
    if rec.target_node.is_null() {
        return Err(AdjacencyError::NullNode);
    }
    if !rec.next_target_edge.is_null() || !rec.prev_target_edge.is_null() {
        return Err(AdjacencyError::AlreadyLinked);
    }

    rec.next_target_edge = head;
    rec.prev_target_edge = SlotRef::NULL;
    view.write(new_edge, &rec);

    if !head.is_null() {
        let mut head_rec = view.read(head).ok_or(AdjacencyError::InvalidSlot)?;
        head_rec.prev_target_edge = new_edge;
        view.write(head, &head_rec);
    }

    Ok(new_edge)
}

/// Unlink `edge` from the source node's adjacency list.
///
/// Patches `prev` and `next` neighbours in O(1).  Returns the new head
/// of the list if the removed edge was the head.
pub fn unlink_source<V: EdgeRecordView>(
    view: &mut V,
    edge: SlotRef,
) -> Result<Option<SlotRef>, AdjacencyError> {
    let rec = view.read(edge).ok_or(AdjacencyError::InvalidSlot)?;
    let prev = rec.prev_source_edge;
    let next = rec.next_source_edge;

    if !prev.is_null() {
        let mut prev_rec = view.read(prev).ok_or(AdjacencyError::InvalidSlot)?;
        prev_rec.next_source_edge = next;
        view.write(prev, &prev_rec);
    }
    if !next.is_null() {
        let mut next_rec = view.read(next).ok_or(AdjacencyError::InvalidSlot)?;
        next_rec.prev_source_edge = prev;
        view.write(next, &next_rec);
    }

    // Clear the removed edge's own pointers.
    let mut cleared = rec;
    cleared.prev_source_edge = SlotRef::NULL;
    cleared.next_source_edge = SlotRef::NULL;
    view.write(edge, &cleared);

    // If this edge was the head, the new head is `next`.
    let new_head = if prev.is_null() {
        Some(next)
    } else {
        None
    };
    Ok(new_head)
}

/// Unlink `edge` from the target node's adjacency list.
///
/// Returns the new head of the list if the removed edge was the head.
pub fn unlink_target<V: EdgeRecordView>(
    view: &mut V,
    edge: SlotRef,
) -> Result<Option<SlotRef>, AdjacencyError> {
    let rec = view.read(edge).ok_or(AdjacencyError::InvalidSlot)?;
    let prev = rec.prev_target_edge;
    let next = rec.next_target_edge;

    if !prev.is_null() {
        let mut prev_rec = view.read(prev).ok_or(AdjacencyError::InvalidSlot)?;
        prev_rec.next_target_edge = next;
        view.write(prev, &prev_rec);
    }
    if !next.is_null() {
        let mut next_rec = view.read(next).ok_or(AdjacencyError::InvalidSlot)?;
        next_rec.prev_target_edge = prev;
        view.write(next, &next_rec);
    }

    let mut cleared = rec;
    cleared.prev_target_edge = SlotRef::NULL;
    cleared.next_target_edge = SlotRef::NULL;
    view.write(edge, &cleared);

    let new_head = if prev.is_null() {
        Some(next)
    } else {
        None
    };
    Ok(new_head)
}

/// Collect all outgoing edge slots for a given head.
pub fn collect_outgoing<V: EdgeRecordView>(view: &V, head: SlotRef) -> Vec<SlotRef> {
    let mut result = Vec::new();
    let mut current = head;
    while !current.is_null() {
        result.push(current);
        match view.read(current) {
            Some(rec) => current = rec.next_source_edge,
            None => break,
        }
    }
    result
}

/// Collect all incoming edge slots for a given head.
pub fn collect_incoming<V: EdgeRecordView>(view: &V, head: SlotRef) -> Vec<SlotRef> {
    let mut result = Vec::new();
    let mut current = head;
    while !current.is_null() {
        result.push(current);
        match view.read(current) {
            Some(rec) => current = rec.next_target_edge,
            None => break,
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::record::{EdgeRecord, SlotRef};
    use std::collections::HashMap;

    /// In-memory map backed edge store for unit tests.
    struct TestStore {
        edges: HashMap<SlotRef, EdgeRecord>,
    }

    impl TestStore {
        fn new() -> Self {
            Self {
                edges: HashMap::new(),
            }
        }

        fn insert(&mut self, slot: SlotRef, rec: EdgeRecord) {
            self.edges.insert(slot, rec);
        }
    }

    impl EdgeRecordView for TestStore {
        fn read(&self, slot: SlotRef) -> Option<EdgeRecord> {
            self.edges.get(&slot).copied()
        }

        fn write(&mut self, slot: SlotRef, record: &EdgeRecord) {
            self.edges.insert(slot, *record);
        }
    }

    #[test]
    fn link_source_head_empty_list() {
        let mut store = TestStore::new();
        let e1 = SlotRef::new(1, 0);
        let n1 = SlotRef::new(10, 0);
        store.insert(e1, EdgeRecord::new(100, 1, 1, 0, n1, SlotRef::NULL));

        let head = link_source_head(&mut store, e1, SlotRef::NULL).unwrap();
        assert_eq!(head, e1);

        let rec = store.read(e1).unwrap();
        assert!(rec.next_source_edge.is_null());
        assert!(rec.prev_source_edge.is_null());
    }

    #[test]
    fn link_source_head_two_edges() {
        let mut store = TestStore::new();
        let n1 = SlotRef::new(10, 0);
        let e1 = SlotRef::new(1, 0);
        let e2 = SlotRef::new(2, 0);

        store.insert(e1, EdgeRecord::new(100, 1, 1, 0, n1, SlotRef::NULL));
        store.insert(e2, EdgeRecord::new(200, 1, 1, 0, n1, SlotRef::NULL));

        // Link e1 first.
        let head = link_source_head(&mut store, e1, SlotRef::NULL).unwrap();
        assert_eq!(head, e1);

        // Link e2 at head.
        let head = link_source_head(&mut store, e2, head).unwrap();
        assert_eq!(head, e2);

        let rec2 = store.read(e2).unwrap();
        assert_eq!(rec2.next_source_edge, e1);
        assert!(rec2.prev_source_edge.is_null());

        let rec1 = store.read(e1).unwrap();
        assert_eq!(rec1.prev_source_edge, e2);
        assert!(rec1.next_source_edge.is_null());
    }

    #[test]
    fn link_target_head_empty_list() {
        let mut store = TestStore::new();
        let e1 = SlotRef::new(1, 0);
        let n1 = SlotRef::new(10, 0);
        store.insert(e1, EdgeRecord::new(100, 1, 0, 1, SlotRef::NULL, n1));

        let head = link_target_head(&mut store, e1, SlotRef::NULL).unwrap();
        assert_eq!(head, e1);
    }

    #[test]
    fn unlink_source_middle() {
        let mut store = TestStore::new();
        let n1 = SlotRef::new(10, 0);
        let e1 = SlotRef::new(1, 0);
        let e2 = SlotRef::new(2, 0);
        let e3 = SlotRef::new(3, 0);

        store.insert(e1, EdgeRecord::new(100, 1, 1, 0, n1, SlotRef::NULL));
        store.insert(e2, EdgeRecord::new(200, 1, 1, 0, n1, SlotRef::NULL));
        store.insert(e3, EdgeRecord::new(300, 1, 1, 0, n1, SlotRef::NULL));

        let _ = link_source_head(&mut store, e1, SlotRef::NULL).unwrap();
        let _head = link_source_head(&mut store, e2, e1).unwrap();
        let _head = link_source_head(&mut store, e3, _head).unwrap();

        // List is now e3 -> e2 -> e1
        let new_head = unlink_source(&mut store, e2).unwrap();
        assert!(new_head.is_none()); // e2 was not the head

        let rec3 = store.read(e3).unwrap();
        assert_eq!(rec3.next_source_edge, e1);

        let rec1 = store.read(e1).unwrap();
        assert_eq!(rec1.prev_source_edge, e3);

        let rec2 = store.read(e2).unwrap();
        assert!(rec2.prev_source_edge.is_null());
        assert!(rec2.next_source_edge.is_null());
    }

    #[test]
    fn unlink_source_head() {
        let mut store = TestStore::new();
        let n1 = SlotRef::new(10, 0);
        let e1 = SlotRef::new(1, 0);
        let e2 = SlotRef::new(2, 0);

        store.insert(e1, EdgeRecord::new(100, 1, 1, 0, n1, SlotRef::NULL));
        store.insert(e2, EdgeRecord::new(200, 1, 1, 0, n1, SlotRef::NULL));

        let _head = link_source_head(&mut store, e1, SlotRef::NULL).unwrap();
        let _head = link_source_head(&mut store, e2, _head).unwrap();

        let new_head = unlink_source(&mut store, e2).unwrap();
        assert_eq!(new_head, Some(e1));

        let rec1 = store.read(e1).unwrap();
        assert!(rec1.prev_source_edge.is_null());
    }

    #[test]
    fn unlink_target_middle() {
        let mut store = TestStore::new();
        let n1 = SlotRef::new(10, 0);
        let e1 = SlotRef::new(1, 0);
        let e2 = SlotRef::new(2, 0);
        let e3 = SlotRef::new(3, 0);

        store.insert(e1, EdgeRecord::new(100, 1, 0, 1, SlotRef::NULL, n1));
        store.insert(e2, EdgeRecord::new(200, 1, 0, 1, SlotRef::NULL, n1));
        store.insert(e3, EdgeRecord::new(300, 1, 0, 1, SlotRef::NULL, n1));

        let _ = link_target_head(&mut store, e1, SlotRef::NULL).unwrap();
        let _head = link_target_head(&mut store, e2, e1).unwrap();
        let _head = link_target_head(&mut store, e3, _head).unwrap();

        let new_head = unlink_target(&mut store, e2).unwrap();
        assert!(new_head.is_none());

        let rec3 = store.read(e3).unwrap();
        assert_eq!(rec3.next_target_edge, e1);
    }

    #[test]
    fn collect_outgoing_traversal() {
        let mut store = TestStore::new();
        let n1 = SlotRef::new(10, 0);
        let e1 = SlotRef::new(1, 0);
        let e2 = SlotRef::new(2, 0);
        let e3 = SlotRef::new(3, 0);

        store.insert(e1, EdgeRecord::new(100, 1, 1, 0, n1, SlotRef::NULL));
        store.insert(e2, EdgeRecord::new(200, 1, 1, 0, n1, SlotRef::NULL));
        store.insert(e3, EdgeRecord::new(300, 1, 1, 0, n1, SlotRef::NULL));

        let _ = link_source_head(&mut store, e1, SlotRef::NULL).unwrap();
        let head = link_source_head(&mut store, e2, e1).unwrap();
        let head = link_source_head(&mut store, e3, head).unwrap();

        let edges = collect_outgoing(&store, head);
        assert_eq!(edges, vec![e3, e2, e1]);
    }

    #[test]
    fn collect_incoming_traversal() {
        let mut store = TestStore::new();
        let n1 = SlotRef::new(10, 0);
        let e1 = SlotRef::new(1, 0);
        let e2 = SlotRef::new(2, 0);

        store.insert(e1, EdgeRecord::new(100, 1, 0, 1, SlotRef::NULL, n1));
        store.insert(e2, EdgeRecord::new(200, 1, 0, 1, SlotRef::NULL, n1));

        let _ = link_target_head(&mut store, e1, SlotRef::NULL).unwrap();
        let head = link_target_head(&mut store, e2, e1).unwrap();

        let edges = collect_incoming(&store, head);
        assert_eq!(edges, vec![e2, e1]);
    }

    #[test]
    fn link_already_linked_fails() {
        let mut store = TestStore::new();
        let n1 = SlotRef::new(10, 0);
        let e1 = SlotRef::new(1, 0);
        let e2 = SlotRef::new(2, 0);
        store.insert(e1, EdgeRecord::new(100, 1, 1, 0, n1, SlotRef::NULL));
        store.insert(e2, EdgeRecord::new(200, 1, 1, 0, n1, SlotRef::NULL));

        // Link e1 first, then e2 (so e1 gets a next pointer).
        let head = link_source_head(&mut store, e1, SlotRef::NULL).unwrap();
        let _head = link_source_head(&mut store, e2, head).unwrap();

        // e1 now has next_source_edge = e2, so re-linking should fail.
        let result = link_source_head(&mut store, e1, SlotRef::NULL);
        assert_eq!(result, Err(AdjacencyError::AlreadyLinked));
    }

    #[test]
    fn link_null_node_fails() {
        let mut store = TestStore::new();
        let e1 = SlotRef::new(1, 0);
        store.insert(e1, EdgeRecord::new(100, 1, 0, 0, SlotRef::NULL, SlotRef::NULL));

        let result = link_source_head(&mut store, e1, SlotRef::NULL);
        assert_eq!(result, Err(AdjacencyError::NullNode));
    }
}
