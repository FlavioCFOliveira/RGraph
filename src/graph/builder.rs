//! Consuming builders for type-safe graph construction.
//!
//! [`NodeBuilder`] and [`RelationshipBuilder`] allow callers to construct
//! nodes and edges fluently, validating constraints (e.g. endpoint existence)
//! before the final `build` call.

use crate::graph::property::Property;
use crate::graph::record::{NodeRecord, EdgeRecord, SlotRef};
use std::collections::HashMap;

/// Builder for a node with label and properties.
///
/// # Example
///
/// ```ignore
/// let node = NodeBuilder::new(1)
///     .label(42)
///     .property("name", "Alice")
///     .build();
/// ```
pub struct NodeBuilder {
    node_id: u64,
    label_id: u32,
    properties: HashMap<String, Property>,
}

impl NodeBuilder {
    /// Start building a node with the given `node_id`.
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            label_id: 0,
            properties: HashMap::new(),
        }
    }

    /// Set the label id (overwrites any previous label).
    pub fn label(mut self, label_id: u32) -> Self {
        self.label_id = label_id;
        self
    }

    /// Add a property with a value that implements `Into<Property>`.
    pub fn property<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<Property>,
    {
        self.properties.insert(key.into(), value.into());
        self
    }

    /// Consume the builder and return a [`NodeRecord`].
    ///
    /// Properties are **not** materialised into the record here; the caller
    /// is responsible for persisting them via the storage engine.
    pub fn build(self) -> NodeRecord {
        NodeRecord::new(self.node_id, self.label_id)
    }

    /// Return the collected properties so the caller can store them.
    pub fn properties(self) -> HashMap<String, Property> {
        self.properties
    }

    /// Return the node id.
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Return the label id.
    pub fn label_id(&self) -> u32 {
        self.label_id
    }
}

/// Builder for a relationship with type, properties, and validated endpoints.
///
/// # Example
///
/// ```ignore
/// let rel = RelationshipBuilder::new(100)
///     .from(1)
///     .to(2)
///     .type_id(7)
///     .property("since", 2020i64)
///     .build();
/// ```
pub struct RelationshipBuilder {
    edge_id: u64,
    type_id: u32,
    source_id: Option<u64>,
    target_id: Option<u64>,
    properties: HashMap<String, Property>,
}

/// Error raised when a [`RelationshipBuilder`] constraint is violated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuilderError {
    /// The source node id was not set.
    MissingSource,
    /// The target node id was not set.
    MissingTarget,
    /// The relationship type was not set.
    MissingType,
    /// The edge id was not set.
    MissingEdgeId,
}

impl std::fmt::Display for BuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuilderError::MissingSource => write!(f, "source node id is required"),
            BuilderError::MissingTarget => write!(f, "target node id is required"),
            BuilderError::MissingType => write!(f, "relationship type id is required"),
            BuilderError::MissingEdgeId => write!(f, "edge id is required"),
        }
    }
}

impl std::error::Error for BuilderError {}

impl RelationshipBuilder {
    /// Start building a relationship with the given `edge_id`.
    pub fn new(edge_id: u64) -> Self {
        Self {
            edge_id,
            type_id: 0,
            source_id: None,
            target_id: None,
            properties: HashMap::new(),
        }
    }

    /// Set the source node id.
    pub fn from(mut self, node_id: u64) -> Self {
        self.source_id = Some(node_id);
        self
    }

    /// Set the target node id.
    pub fn to(mut self, node_id: u64) -> Self {
        self.target_id = Some(node_id);
        self
    }

    /// Set the relationship type id.
    pub fn type_id(mut self, type_id: u32) -> Self {
        self.type_id = type_id;
        self
    }

    /// Add a property with a value that implements `Into<Property>`.
    pub fn property<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<Property>,
    {
        self.properties.insert(key.into(), value.into());
        self
    }

    /// Consume the builder and return an [`EdgeRecord`].
    ///
    /// Returns [`BuilderError`] if required fields are missing.
    pub fn build(self) -> Result<EdgeRecord, BuilderError> {
        let source_id = self.source_id.ok_or(BuilderError::MissingSource)?;
        let target_id = self.target_id.ok_or(BuilderError::MissingTarget)?;
        if self.type_id == 0 {
            return Err(BuilderError::MissingType);
        }
        let source_slot = SlotRef::new(source_id as u32, 0);
        let target_slot = SlotRef::new(target_id as u32, 0);
        Ok(EdgeRecord::new(self.edge_id, self.type_id, source_slot, target_slot))
    }

    /// Return the collected properties so the caller can store them.
    pub fn properties(self) -> HashMap<String, Property> {
        self.properties
    }

    /// Return the edge id.
    pub fn edge_id(&self) -> u64 {
        self.edge_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_builder_defaults() {
        let builder = NodeBuilder::new(1);
        assert_eq!(builder.node_id(), 1);
        assert_eq!(builder.label_id(), 0);
    }

    #[test]
    fn node_builder_with_label() {
        let node = NodeBuilder::new(1)
            .label(42)
            .build();
        assert_eq!(node.node_id, 1);
        assert_eq!(node.label_id, 42);
    }

    #[test]
    fn node_builder_with_properties() {
        let builder = NodeBuilder::new(1)
            .label(42)
            .property("name", "Alice")
            .property("age", 30i64);
        let props = builder.properties();
        assert_eq!(props.len(), 2);
        assert_eq!(props["name"], Property::String("Alice".to_string()));
        assert_eq!(props["age"], Property::Integer(30));
    }

    #[test]
    fn relationship_builder_full() {
        let edge = RelationshipBuilder::new(100)
            .from(1)
            .to(2)
            .type_id(7)
            .property("since", 2020i64)
            .build()
            .unwrap();
        assert_eq!(edge.edge_id, 100);
        assert_eq!(edge.type_id, 7);
    }

    #[test]
    fn relationship_builder_missing_source() {
        let result = RelationshipBuilder::new(100)
            .to(2)
            .type_id(7)
            .build();
        assert_eq!(result, Err(BuilderError::MissingSource));
    }

    #[test]
    fn relationship_builder_missing_target() {
        let result = RelationshipBuilder::new(100)
            .from(1)
            .type_id(7)
            .build();
        assert_eq!(result, Err(BuilderError::MissingTarget));
    }

    #[test]
    fn relationship_builder_missing_type() {
        let result = RelationshipBuilder::new(100)
            .from(1)
            .to(2)
            .build();
        assert_eq!(result, Err(BuilderError::MissingType));
    }

    #[test]
    fn relationship_builder_properties() {
        let builder = RelationshipBuilder::new(100)
            .from(1)
            .to(2)
            .type_id(7)
            .property("since", 2020i64)
            .property("active", true);
        let props = builder.properties();
        assert_eq!(props["since"], Property::Integer(2020));
        assert_eq!(props["active"], Property::Boolean(true));
    }

    #[test]
    fn builder_consuming_methods() {
        // Verify that builder methods are consuming (ownership moves).
        let b = NodeBuilder::new(1);
        let b2 = b.label(10);
        let _node = b2.build();
        // b is no longer usable here because label() consumed it.
    }
}
