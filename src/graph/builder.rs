//! Consuming builders for type-safe graph construction.
//!
//! [`NodeBuilder`] and [`RelationshipBuilder`] allow callers to construct
//! nodes and edges fluently, validating constraints (e.g. endpoint existence)
//! before the final `build` call.
//!
//! Node and edge ids are **server-allocated**: the builder does not accept
//! an id and `Graph::create_node` / `Graph::create_relationship` fill it in
//! via the engine's [`IdAllocator`](crate::graph::engine::IdAllocator).

use crate::graph::property::Property;
use crate::graph::record::{NodeRecord, EdgeRecord, SlotRef};
use std::collections::HashMap;

/// Builder for a node with label and properties.
///
/// # Example
///
/// ```ignore
/// let node_builder = NodeBuilder::new()
///     .label(42)
///     .property("name", "Alice");
/// // id is allocated by the engine during create_node
/// ```
pub struct NodeBuilder {
    label_id: u32,
    properties: HashMap<String, Property>,
}

impl NodeBuilder {
    /// Start building a node.  The node id will be assigned server-side by
    /// [`Graph::create_node`](crate::graph::graph::Graph::create_node).
    pub fn new() -> Self {
        Self {
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

    /// Consume the builder and return a [`NodeRecord`] with `node_id = 0`.
    ///
    /// The caller **must** overwrite `node_id` with the server-allocated id
    /// before persisting the record.  Properties are returned separately via
    /// [`NodeBuilder::into_parts`].
    pub fn build(self) -> NodeRecord {
        NodeRecord::new(0, self.label_id)
    }

    /// Consume the builder and return the record together with its properties.
    pub fn into_parts(self) -> (NodeRecord, HashMap<String, Property>) {
        let record = NodeRecord::new(0, self.label_id);
        (record, self.properties)
    }

    /// Return the collected properties so the caller can store them (consuming).
    pub fn properties(self) -> HashMap<String, Property> {
        self.properties
    }

    /// Return the label id.
    pub fn label_id(&self) -> u32 {
        self.label_id
    }
}

impl Default for NodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for a relationship with type, properties, and validated endpoints.
///
/// The edge id is **server-allocated**: do not provide it here.
/// [`Graph::create_relationship`](crate::graph::graph::Graph::create_relationship)
/// assigns the id and resolves the physical slot references from the logical
/// `source_id` / `target_id`.
///
/// # Example
///
/// ```ignore
/// let rel = RelationshipBuilder::new()
///     .from(src_id)
///     .to(tgt_id)
///     .type_id(7)
///     .property("since", 2020i64);
/// // edge id is allocated by the engine during create_relationship
/// ```
pub struct RelationshipBuilder {
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
}

impl std::fmt::Display for BuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuilderError::MissingSource => write!(f, "source node id is required"),
            BuilderError::MissingTarget => write!(f, "target node id is required"),
            BuilderError::MissingType => write!(f, "relationship type id is required"),
        }
    }
}

impl std::error::Error for BuilderError {}

impl RelationshipBuilder {
    /// Start building a relationship.  The edge id will be assigned server-side.
    pub fn new() -> Self {
        Self {
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

    /// Consume the builder and return an [`EdgeRecord`] with `edge_id = 0`
    /// and `source_node` / `target_node` set to `SlotRef::NULL`.
    ///
    /// The caller **must** overwrite `edge_id`, `source_node`, and
    /// `target_node` before persisting.  Use
    /// [`RelationshipBuilder::into_parts`] to also retrieve the properties.
    ///
    /// # Errors
    ///
    /// Returns [`BuilderError`] if the source, target, or type are missing.
    pub fn build(self) -> Result<EdgeRecord, BuilderError> {
        let source_id = self.source_id.ok_or(BuilderError::MissingSource)?;
        let target_id = self.target_id.ok_or(BuilderError::MissingTarget)?;
        if self.type_id == 0 {
            return Err(BuilderError::MissingType);
        }
        // Physical slots are NULL here; the engine resolves them from the
        // logical ids via `lookup_node_slot`.
        Ok(EdgeRecord::new(
            0,
            self.type_id,
            source_id,
            target_id,
            SlotRef::NULL,
            SlotRef::NULL,
        ))
    }

    /// Consume the builder and return the record together with its properties.
    ///
    /// # Errors
    ///
    /// Same as [`RelationshipBuilder::build`].
    pub fn into_parts(self) -> Result<(EdgeRecord, HashMap<String, Property>), BuilderError> {
        let props = self.properties.clone();
        let record = RelationshipBuilder {
            type_id: self.type_id,
            source_id: self.source_id,
            target_id: self.target_id,
            properties: HashMap::new(),
        }.build()?;
        Ok((record, props))
    }

    /// Return the collected properties so the caller can store them (consuming).
    pub fn properties(self) -> HashMap<String, Property> {
        self.properties
    }
}

impl Default for RelationshipBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_builder_defaults() {
        let builder = NodeBuilder::new();
        assert_eq!(builder.label_id(), 0);
    }

    #[test]
    fn node_builder_with_label() {
        // id is 0 until server allocates it
        let node = NodeBuilder::new()
            .label(42)
            .build();
        assert_eq!(node.node_id, 0);
        assert_eq!(node.label_id, 42);
    }

    #[test]
    fn node_builder_with_properties() {
        let builder = NodeBuilder::new()
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
        let edge = RelationshipBuilder::new()
            .from(1)
            .to(2)
            .type_id(7)
            .property("since", 2020i64)
            .build()
            .unwrap();
        // edge_id = 0 until server allocates it
        assert_eq!(edge.edge_id, 0);
        assert_eq!(edge.type_id, 7);
        assert_eq!(edge.source_id, 1);
        assert_eq!(edge.target_id, 2);
        // physical slots are NULL until engine resolves them
        assert!(edge.source_node.is_null());
        assert!(edge.target_node.is_null());
    }

    #[test]
    fn relationship_builder_missing_source() {
        let result = RelationshipBuilder::new()
            .to(2)
            .type_id(7)
            .build();
        assert_eq!(result, Err(BuilderError::MissingSource));
    }

    #[test]
    fn relationship_builder_missing_target() {
        let result = RelationshipBuilder::new()
            .from(1)
            .type_id(7)
            .build();
        assert_eq!(result, Err(BuilderError::MissingTarget));
    }

    #[test]
    fn relationship_builder_missing_type() {
        let result = RelationshipBuilder::new()
            .from(1)
            .to(2)
            .build();
        assert_eq!(result, Err(BuilderError::MissingType));
    }

    #[test]
    fn relationship_builder_properties() {
        let builder = RelationshipBuilder::new()
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
        let b = NodeBuilder::new();
        let b2 = b.label(10);
        let _node = b2.build();
        // b is no longer usable here because label() consumed it.
    }
}
