//! Composite key encoding for all index types.
//!
//! Keys are encoded as byte strings that preserve lexicographic ordering.
//! This enables efficient range scans.
//!
//! # Key Layouts
//!
//! ## Node ID key (primary index)
//! ```text
//! | node_id: u128 (16 BE bytes) |
//! ```
//!
//! ## Edge adjacency key
//! ```text
//! | source_id: u128 (16 BE) | type_id: u64 (8 BE) | target_id: u128 (16 BE) |
//! ```
//! Total: 40 bytes.
//!
//! ## Label index key
//! ```text
//! | label_hash: u64 (8 BE) | node_id: u128 (16 BE) |
//! ```
//! Total: 24 bytes.
//!
//! ## Property index key
//! ```text
//! | property_id: u64 (8 BE) | value_hash: u64 (8 BE) | node_id: u128 (16 BE) |
//! ```
//! Total: 32 bytes.
//!
//! ## RDF triple key (SPO)
//! ```text
//! | subject: u128 (16 BE) | predicate: u64 (8 BE) | object: u128 (16 BE) |
//! ```
//! Total: 40 bytes.
//!
//! All multi-byte fields are big-endian so that lexicographic byte order
//! matches numeric order.
//!
//! # Variable-length storage
//!
//! [`CompositeKey`] stores its bytes in a [`SmallVec`] with an inline capacity
//! of [`MAX_KEY_LEN`] bytes.  Keys up to that length live entirely on the stack
//! (no allocation); longer keys (long property values, long RDF literals) spill
//! to the heap **without truncation**, so ordering and uniqueness are preserved
//! for keys of any length.  `SmallVec`'s `Ord`/`PartialOrd` compare the byte
//! contents element-wise, identical to comparing the underlying slices.

use smallvec::SmallVec;

/// Inline capacity, in bytes, for a composite key before it spills to the heap.
///
/// Chosen to cover the largest fixed-layout key (40 bytes: adjacency / RDF SPO)
/// so common keys never allocate.  It is **not** a maximum: longer keys are
/// stored on the heap.
pub const MAX_KEY_LEN: usize = 40;

/// Backing storage for a [`CompositeKey`]: inline up to [`MAX_KEY_LEN`] bytes,
/// heap-allocated beyond.
pub type KeyBytes = SmallVec<[u8; MAX_KEY_LEN]>;

/// A composite key with variable length.
///
/// Short keys are stored inline; long keys spill to the heap.  Lexicographic
/// byte order is preserved by `SmallVec`'s element-wise comparison.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompositeKey {
    bytes: KeyBytes,
}

impl CompositeKey {
    /// Create an empty key.
    pub fn empty() -> Self {
        Self {
            bytes: SmallVec::new(),
        }
    }

    /// Create from a byte slice.  The full slice is stored without truncation.
    pub fn from_slice(data: &[u8]) -> Self {
        Self {
            bytes: SmallVec::from_slice(data),
        }
    }

    /// Create from owned bytes.
    pub fn from_bytes(bytes: KeyBytes) -> Self {
        Self { bytes }
    }

    /// Return the key bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Compare two keys lexicographically.
    pub fn cmp_keys(a: &Self, b: &Self) -> std::cmp::Ordering {
        a.as_slice().cmp(b.as_slice())
    }

    /// Length of the key in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// A small builder that accumulates key bytes, used by the fixed-layout key
/// constructors below.  Encapsulates the inline-vs-heap storage decision.
struct KeyBuilder {
    bytes: KeyBytes,
}

impl KeyBuilder {
    fn new() -> Self {
        Self {
            bytes: SmallVec::new(),
        }
    }

    fn push_u64_be(&mut self, v: u64) {
        self.bytes.extend_from_slice(&v.to_be_bytes());
    }

    fn push_u128_be(&mut self, v: u128) {
        self.bytes.extend_from_slice(&v.to_be_bytes());
    }

    fn push_slice(&mut self, s: &[u8]) {
        self.bytes.extend_from_slice(s);
    }

    fn finish(self) -> CompositeKey {
        CompositeKey { bytes: self.bytes }
    }
}

/// Trait for types that can be encoded into a composite key.
pub trait KeyEncoder {
    /// Encode into `out`, returning the number of bytes written.
    fn encode(&self, out: &mut [u8]) -> usize;
}

/// Encode a `u64` in big-endian into `out` and return 8.
pub fn encode_u64_be(v: u64, out: &mut [u8]) -> usize {
    out[..8].copy_from_slice(&v.to_be_bytes());
    8
}

/// Encode a `u128` in big-endian into `out` and return 16.
pub fn encode_u128_be(v: u128, out: &mut [u8]) -> usize {
    out[..16].copy_from_slice(&v.to_be_bytes());
    16
}

/// Decode a `u64` from big-endian bytes.
pub fn decode_u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// Decode a `u128` from big-endian bytes.
pub fn decode_u128_be(bytes: &[u8]) -> u128 {
    let mut arr = [0u8; 16];
    arr.copy_from_slice(bytes);
    u128::from_be_bytes(arr)
}

/// Build a node-id primary key.
pub fn node_id_key(node_id: u128) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u128_be(node_id);
    b.finish()
}

/// Build an edge-id primary key.
pub fn edge_id_key(edge_id: u128) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u128_be(edge_id);
    b.finish()
}

/// Build an edge adjacency key.
pub fn edge_adjacency_key(source_id: u128, type_id: u64, target_id: u128) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u128_be(source_id);
    b.push_u64_be(type_id);
    b.push_u128_be(target_id);
    b.finish()
}

/// Build a label index key.
pub fn label_index_key(label_hash: u64, node_id: u128) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u64_be(label_hash);
    b.push_u128_be(node_id);
    b.finish()
}

/// Build a type index key for edge type lookups.
pub fn type_index_key(type_id: u64, edge_id: u128) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u64_be(type_id);
    b.push_u128_be(edge_id);
    b.finish()
}

/// Build a property index key.
///
/// Layout: `property_id (8 BE) | serialized_value (variable) | entity_id (16 BE)`.
/// The serialized value is stored **in full** — no truncation — so ordering and
/// uniqueness are preserved for property values of any length.  Because the
/// value is variable-length, the trailing `entity_id` is unambiguous only when
/// callers serialize values order-preservingly (see [`value_codec`]); that is
/// the contract for the property index.
///
/// [`value_codec`]: crate::index::value_codec
pub fn property_index_key(
    property_id: u64,
    serialized_value: &[u8],
    entity_id: u128,
) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u64_be(property_id);
    b.push_slice(serialized_value);
    b.push_u128_be(entity_id);
    b.finish()
}

/// Build an RDF SPO triple key.
pub fn rdf_spo_key(subject: u128, predicate: u64, object: u128) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u128_be(subject);
    b.push_u64_be(predicate);
    b.push_u128_be(object);
    b.finish()
}

/// Build an RDF POS triple key.
pub fn rdf_pos_key(predicate: u64, object: u128, subject: u128) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u64_be(predicate);
    b.push_u128_be(object);
    b.push_u128_be(subject);
    b.finish()
}

/// Build an RDF OSP triple key.
pub fn rdf_osp_key(object: u128, subject: u128, predicate: u64) -> CompositeKey {
    let mut b = KeyBuilder::new();
    b.push_u128_be(object);
    b.push_u128_be(subject);
    b.push_u64_be(predicate);
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_key_order() {
        let a = node_id_key(1);
        let b = node_id_key(2);
        assert!(a.as_slice() < b.as_slice());
    }

    #[test]
    fn edge_adjacency_key_order() {
        let a = edge_adjacency_key(1, 10, 100);
        let b = edge_adjacency_key(1, 10, 101);
        let c = edge_adjacency_key(1, 11, 0);
        let d = edge_adjacency_key(2, 0, 0);
        assert!(a.as_slice() < b.as_slice());
        assert!(b.as_slice() < c.as_slice());
        assert!(c.as_slice() < d.as_slice());
    }

    #[test]
    fn label_index_key_order() {
        let a = label_index_key(1, 10);
        let b = label_index_key(1, 11);
        let c = label_index_key(2, 0);
        assert!(a.as_slice() < b.as_slice());
        assert!(b.as_slice() < c.as_slice());
    }

    #[test]
    fn edge_id_key_order() {
        let a = edge_id_key(1);
        let b = edge_id_key(2);
        assert!(a.as_slice() < b.as_slice());
    }

    #[test]
    fn type_index_key_order() {
        let a = type_index_key(1, 10);
        let b = type_index_key(1, 11);
        let c = type_index_key(2, 0);
        assert!(a.as_slice() < b.as_slice());
        assert!(b.as_slice() < c.as_slice());
    }

    #[test]
    fn property_index_key_order() {
        let a = property_index_key(1, &[10u8], 100);
        let b = property_index_key(1, &[10u8], 101);
        let c = property_index_key(1, &[11u8], 0);
        let d = property_index_key(2, &[], 0);
        assert!(a.as_slice() < b.as_slice());
        assert!(b.as_slice() < c.as_slice());
        assert!(c.as_slice() < d.as_slice());
    }

    #[test]
    fn rdf_spo_key_order() {
        let a = rdf_spo_key(1, 10, 100);
        let b = rdf_spo_key(1, 10, 101);
        let c = rdf_spo_key(1, 11, 0);
        let d = rdf_spo_key(2, 0, 0);
        assert!(a.as_slice() < b.as_slice());
        assert!(b.as_slice() < c.as_slice());
        assert!(c.as_slice() < d.as_slice());
    }

    #[test]
    fn composite_key_roundtrip() {
        let k = edge_adjacency_key(0xDEADBEEF, 0xCAFE, 0xBABE);
        assert_eq!(k.len(), 40);
        let buf = k.as_slice();
        assert_eq!(decode_u128_be(&buf[0..16]), 0xDEADBEEF);
        assert_eq!(decode_u64_be(&buf[16..24]), 0xCAFE);
        assert_eq!(decode_u128_be(&buf[24..40]), 0xBABE);
    }

    #[test]
    fn empty_key_is_empty() {
        let k = CompositeKey::empty();
        assert!(k.is_empty());
        assert_eq!(k.len(), 0);
    }

    #[test]
    fn from_slice_preserves_long_keys() {
        // Keys longer than the inline capacity must be stored in full (no
        // truncation) so ordering and uniqueness hold.
        let long = vec![0xAB; MAX_KEY_LEN + 10];
        let k = CompositeKey::from_slice(&long);
        assert_eq!(k.len(), MAX_KEY_LEN + 10);
        assert_eq!(k.as_slice(), long.as_slice());
    }

    #[test]
    fn long_property_keys_round_trip_and_order() {
        // Two distinct long values that share a 60-byte prefix must remain
        // distinct and correctly ordered — the old 40-byte buffer truncated
        // both to the same key, collapsing them.
        let mut v1 = vec![0x10u8; 60];
        let mut v2 = v1.clone();
        v1.push(0x01);
        v2.push(0x02);
        let k1 = property_index_key(7, &v1, 100);
        let k2 = property_index_key(7, &v2, 100);
        assert_ne!(k1, k2, "long values must not collapse to the same key");
        assert!(k1.as_slice() < k2.as_slice(), "ordering must follow value bytes");
        // Round-trip: the serialized value is recoverable between the 8-byte
        // property id and the trailing 16-byte entity id.
        let buf = k1.as_slice();
        assert_eq!(decode_u64_be(&buf[0..8]), 7);
        let value = &buf[8..buf.len() - 16];
        assert_eq!(value, v1.as_slice());
        assert_eq!(decode_u128_be(&buf[buf.len() - 16..]), 100);
    }

    #[test]
    fn prefix_order() {
        // All keys sharing the same prefix should sort by the next field.
        let k1 = property_index_key(5, &[100u8], 1);
        let k2 = property_index_key(5, &[100u8], 2);
        let k3 = property_index_key(5, &[101u8], 0);
        assert!(k1.as_slice() < k2.as_slice());
        assert!(k2.as_slice() < k3.as_slice());
    }
}
