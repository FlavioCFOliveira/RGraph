//! Schema catalog — tracks all labels, relationship types, and property keys
//! defined in the graph, mapping each to a compact u32 id.
//!
//! The catalog is the single source of truth for schema identifiers.  Every
//! label, relationship type, and property key is registered here on first use
//! and subsequently referenced by its stable u32 id.  The id space is dense
//! starting at 1 (id 0 is reserved as a sentinel meaning "no schema element").
//!
//! # Persistence
//!
//! The catalog is currently an in-memory data structure seeded from a snapshot
//! that can be encoded to and decoded from bytes.  The snapshot is intended to
//! be stored in the engine's superblock region; full WAL-logged persistence is
//! deferred to Sprint D.
//!
//! # Thread safety
//!
//! The catalog does not provide its own locks.  Callers that need concurrent
//! access should wrap it in an `Arc<RwLock<Catalog>>` (the pattern used in
//! `GraphStorageEngine`).

use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
// Catalog
// ─────────────────────────────────────────────────────────────────────────────

/// Schema catalog for a single graph database.
///
/// Maps human-readable names to compact u32 ids for labels, relationship
/// types, and property keys.  Id 0 is reserved; real ids start at 1.
///
/// # Examples
///
/// ```
/// use rgraph::catalog::Catalog;
///
/// let mut catalog = Catalog::new();
/// let label_id = catalog.get_or_create_label("Person");
/// assert_eq!(catalog.label_id("Person"), Some(label_id));
/// ```
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Label name → u32 id.
    labels: HashMap<String, u32>,
    /// Reverse: label id → name.
    labels_rev: HashMap<u32, String>,
    /// Next label id to assign.
    next_label_id: u32,

    /// Relationship-type name → u32 id.
    rel_types: HashMap<String, u32>,
    /// Reverse: rel_type id → name.
    rel_types_rev: HashMap<u32, String>,
    /// Next relationship-type id to assign.
    next_rel_type_id: u32,

    /// Property-key name → u32 id.
    property_keys: HashMap<String, u32>,
    /// Reverse: property_key id → name.
    property_keys_rev: HashMap<u32, String>,
    /// Next property-key id to assign.
    next_property_key_id: u32,
}

impl Catalog {
    /// Create an empty catalog (all id sequences start at 1).
    pub fn new() -> Self {
        Self {
            next_label_id: 1,
            next_rel_type_id: 1,
            next_property_key_id: 1,
            ..Default::default()
        }
    }

    // ── Labels ────────────────────────────────────────────────────────────

    /// Return the id for `label`, creating it if it does not yet exist.
    pub fn get_or_create_label(&mut self, label: &str) -> u32 {
        if let Some(&id) = self.labels.get(label) {
            return id;
        }
        let id = self.next_label_id;
        self.next_label_id += 1;
        self.labels.insert(label.to_string(), id);
        self.labels_rev.insert(id, label.to_string());
        id
    }

    /// Look up the id for an existing label.  Returns `None` if unknown.
    pub fn label_id(&self, label: &str) -> Option<u32> {
        self.labels.get(label).copied()
    }

    /// Look up the name for a label id.  Returns `None` if unknown.
    pub fn label_name(&self, id: u32) -> Option<&str> {
        self.labels_rev.get(&id).map(|s| s.as_str())
    }

    /// Iterate over all registered labels as `(name, id)` pairs.
    pub fn all_labels(&self) -> impl Iterator<Item = (&str, u32)> {
        self.labels.iter().map(|(k, &v)| (k.as_str(), v))
    }

    // ── Relationship types ────────────────────────────────────────────────

    /// Return the id for `rel_type`, creating it if it does not yet exist.
    pub fn get_or_create_rel_type(&mut self, rel_type: &str) -> u32 {
        if let Some(&id) = self.rel_types.get(rel_type) {
            return id;
        }
        let id = self.next_rel_type_id;
        self.next_rel_type_id += 1;
        self.rel_types.insert(rel_type.to_string(), id);
        self.rel_types_rev.insert(id, rel_type.to_string());
        id
    }

    /// Look up the id for an existing relationship type.  Returns `None` if unknown.
    pub fn rel_type_id(&self, rel_type: &str) -> Option<u32> {
        self.rel_types.get(rel_type).copied()
    }

    /// Look up the name for a relationship type id.  Returns `None` if unknown.
    pub fn rel_type_name(&self, id: u32) -> Option<&str> {
        self.rel_types_rev.get(&id).map(|s| s.as_str())
    }

    /// Iterate over all registered relationship types as `(name, id)` pairs.
    pub fn all_rel_types(&self) -> impl Iterator<Item = (&str, u32)> {
        self.rel_types.iter().map(|(k, &v)| (k.as_str(), v))
    }

    // ── Property keys ─────────────────────────────────────────────────────

    /// Return the id for `property_key`, creating it if it does not yet exist.
    pub fn get_or_create_property_key(&mut self, key: &str) -> u32 {
        if let Some(&id) = self.property_keys.get(key) {
            return id;
        }
        let id = self.next_property_key_id;
        self.next_property_key_id += 1;
        self.property_keys.insert(key.to_string(), id);
        self.property_keys_rev.insert(id, key.to_string());
        id
    }

    /// Look up the id for an existing property key.  Returns `None` if unknown.
    pub fn property_key_id(&self, key: &str) -> Option<u32> {
        self.property_keys.get(key).copied()
    }

    /// Look up the name for a property key id.  Returns `None` if unknown.
    pub fn property_key_name(&self, id: u32) -> Option<&str> {
        self.property_keys_rev.get(&id).map(|s| s.as_str())
    }

    /// Iterate over all registered property keys as `(name, id)` pairs.
    pub fn all_property_keys(&self) -> impl Iterator<Item = (&str, u32)> {
        self.property_keys.iter().map(|(k, &v)| (k.as_str(), v))
    }

    // ── Serialisation ─────────────────────────────────────────────────────

    /// Encode the catalog to a byte vector (little-endian, length-prefixed).
    ///
    /// Format per section (labels, rel_types, property_keys):
    /// ```text
    /// count    : u32 LE
    /// [entry]* : (id: u32 LE, name_len: u16 LE, name: UTF-8 bytes)
    /// ```
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_section(&mut buf, &self.labels);
        encode_section(&mut buf, &self.rel_types);
        encode_section(&mut buf, &self.property_keys);
        buf
    }

    /// Decode a catalog from bytes produced by [`encode`].
    ///
    /// # Errors
    ///
    /// Returns `None` if the bytes are truncated or malformed.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut pos = 0;
        let (labels, labels_rev, next_label_id) = decode_section(bytes, &mut pos)?;
        let (rel_types, rel_types_rev, next_rel_type_id) = decode_section(bytes, &mut pos)?;
        let (property_keys, property_keys_rev, next_property_key_id) = decode_section(bytes, &mut pos)?;
        Some(Self {
            labels,
            labels_rev,
            next_label_id,
            rel_types,
            rel_types_rev,
            next_rel_type_id,
            property_keys,
            property_keys_rev,
            next_property_key_id,
        })
    }
}

fn encode_section(buf: &mut Vec<u8>, map: &HashMap<String, u32>) {
    buf.extend_from_slice(&(map.len() as u32).to_le_bytes());
    for (name, &id) in map {
        buf.extend_from_slice(&id.to_le_bytes());
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
}

/// A decoded catalog section: the forward `name -> id` map, the reverse
/// `id -> name` map, and the next free id.
type DecodedSection = (HashMap<String, u32>, HashMap<u32, String>, u32);

fn decode_section(bytes: &[u8], pos: &mut usize) -> Option<DecodedSection> {
    let count = read_u32_le(bytes, pos)? as usize;
    let mut map = HashMap::with_capacity(count);
    let mut rev = HashMap::with_capacity(count);
    let mut max_id = 0u32;
    for _ in 0..count {
        let id = read_u32_le(bytes, pos)?;
        let name_len = read_u16_le(bytes, pos)? as usize;
        if *pos + name_len > bytes.len() {
            return None;
        }
        let name = std::str::from_utf8(&bytes[*pos..*pos + name_len]).ok()?.to_string();
        *pos += name_len;
        if id > max_id {
            max_id = id;
        }
        map.insert(name.clone(), id);
        rev.insert(id, name);
    }
    Some((map, rev, max_id + 1))
}

fn read_u32_le(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > bytes.len() {
        return None;
    }
    let val = u32::from_le_bytes([bytes[*pos], bytes[*pos + 1], bytes[*pos + 2], bytes[*pos + 3]]);
    *pos += 4;
    Some(val)
}

fn read_u16_le(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    if *pos + 2 > bytes.len() {
        return None;
    }
    let val = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
    *pos += 2;
    Some(val)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_get_or_create_idempotent() {
        let mut c = Catalog::new();
        let id1 = c.get_or_create_label("Person");
        let id2 = c.get_or_create_label("Person");
        assert_eq!(id1, id2);
        assert_ne!(id1, 0, "id 0 is reserved");
    }

    #[test]
    fn distinct_labels_get_distinct_ids() {
        let mut c = Catalog::new();
        let person = c.get_or_create_label("Person");
        let movie = c.get_or_create_label("Movie");
        assert_ne!(person, movie);
    }

    #[test]
    fn label_id_returns_none_for_unknown() {
        let c = Catalog::new();
        assert_eq!(c.label_id("Unknown"), None);
    }

    #[test]
    fn label_name_roundtrip() {
        let mut c = Catalog::new();
        let id = c.get_or_create_label("Actor");
        assert_eq!(c.label_name(id), Some("Actor"));
    }

    #[test]
    fn rel_type_registration() {
        let mut c = Catalog::new();
        let id = c.get_or_create_rel_type("KNOWS");
        assert_eq!(c.rel_type_id("KNOWS"), Some(id));
        assert_eq!(c.rel_type_name(id), Some("KNOWS"));
    }

    #[test]
    fn property_key_registration() {
        let mut c = Catalog::new();
        let id = c.get_or_create_property_key("name");
        assert_eq!(c.property_key_id("name"), Some(id));
        assert_eq!(c.property_key_name(id), Some("name"));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut c = Catalog::new();
        c.get_or_create_label("Person");
        c.get_or_create_label("Movie");
        c.get_or_create_rel_type("KNOWS");
        c.get_or_create_property_key("name");
        c.get_or_create_property_key("age");

        let encoded = c.encode();
        let decoded = Catalog::decode(&encoded).expect("decode should succeed");

        assert_eq!(decoded.label_id("Person"), c.label_id("Person"));
        assert_eq!(decoded.label_id("Movie"), c.label_id("Movie"));
        assert_eq!(decoded.rel_type_id("KNOWS"), c.rel_type_id("KNOWS"));
        assert_eq!(decoded.property_key_id("name"), c.property_key_id("name"));
        assert_eq!(decoded.property_key_id("age"), c.property_key_id("age"));
    }

    #[test]
    fn new_ids_after_decode_do_not_collide() {
        let mut c = Catalog::new();
        c.get_or_create_label("A");
        let encoded = c.encode();
        let mut decoded = Catalog::decode(&encoded).unwrap();

        let new_id = decoded.get_or_create_label("B");
        assert!(new_id > decoded.label_id("A").unwrap(),
            "new id must be greater than any existing id");
    }
}
