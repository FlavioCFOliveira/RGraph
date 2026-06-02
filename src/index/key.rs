//! Composite key encoding for all index types.
//!
//! Keys are encoded as byte strings that preserve lexicographic ordering.
//! This enables efficient range scans and prefix compression.
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

/// Fixed-size buffer large enough for any composite key (max 40 bytes).
pub const MAX_KEY_LEN: usize = 40;

/// A composite key stored inline on the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompositeKey {
    pub bytes: [u8; MAX_KEY_LEN],
    pub len: u8,
}

impl CompositeKey {
    /// Create an empty key.
    pub fn empty() -> Self {
        Self {
            bytes: [0; MAX_KEY_LEN],
            len: 0,
        }
    }

    /// Create from a byte slice (truncates at `MAX_KEY_LEN`).
    pub fn from_slice(data: &[u8]) -> Self {
        let len = data.len().min(MAX_KEY_LEN);
        let mut bytes = [0; MAX_KEY_LEN];
        bytes[..len].copy_from_slice(&data[..len]);
        Self {
            bytes,
            len: len as u8,
        }
    }

    /// Return the active prefix of the key.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// Compare two keys lexicographically.
    pub fn cmp_keys(a: &Self, b: &Self) -> std::cmp::Ordering {
        a.as_slice().cmp(b.as_slice())
    }

    /// Length of the key in bytes.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
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
    let mut bytes = [0; MAX_KEY_LEN];
    encode_u128_be(node_id, &mut bytes);
    CompositeKey { bytes, len: 16 }
}

/// Build an edge-id primary key.
pub fn edge_id_key(edge_id: u128) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    encode_u128_be(edge_id, &mut bytes);
    CompositeKey { bytes, len: 16 }
}

/// Build an edge adjacency key.
pub fn edge_adjacency_key(source_id: u128, type_id: u64, target_id: u128) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    let mut off = 0;
    off += encode_u128_be(source_id, &mut bytes[off..]);
    off += encode_u64_be(type_id, &mut bytes[off..]);
    off += encode_u128_be(target_id, &mut bytes[off..]);
    CompositeKey { bytes, len: off as u8 }
}

/// Build a label index key.
pub fn label_index_key(label_hash: u64, node_id: u128) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    let mut off = 0;
    off += encode_u64_be(label_hash, &mut bytes[off..]);
    off += encode_u128_be(node_id, &mut bytes[off..]);
    CompositeKey { bytes, len: off as u8 }
}

/// Build a type index key for edge type lookups.
pub fn type_index_key(type_id: u64, edge_id: u128) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    let mut off = 0;
    off += encode_u64_be(type_id, &mut bytes[off..]);
    off += encode_u128_be(edge_id, &mut bytes[off..]);
    CompositeKey { bytes, len: off as u8 }
}

/// Build a property index key.
///
/// Layout: `property_id (8 BE) | serialized_value (up to 16 BE) | entity_id (16 BE)`.
/// The serialized value is truncated to fit within [`MAX_KEY_LEN`] (40 bytes).
/// This preserves enough ordering for practical range scans while keeping
/// the key size fixed.
pub fn property_index_key(property_id: u64, serialized_value: &[u8], entity_id: u128) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    let mut off = 0;
    off += encode_u64_be(property_id, &mut bytes[off..]);
    let value_len = serialized_value.len().min(MAX_KEY_LEN - off - 16);
    bytes[off..off + value_len].copy_from_slice(&serialized_value[..value_len]);
    off += value_len;
    off += encode_u128_be(entity_id, &mut bytes[off..]);
    CompositeKey { bytes, len: off as u8 }
}

/// Build an RDF SPO triple key.
pub fn rdf_spo_key(subject: u128, predicate: u64, object: u128) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    let mut off = 0;
    off += encode_u128_be(subject, &mut bytes[off..]);
    off += encode_u64_be(predicate, &mut bytes[off..]);
    off += encode_u128_be(object, &mut bytes[off..]);
    CompositeKey { bytes, len: off as u8 }
}

/// Build an RDF POS triple key.
pub fn rdf_pos_key(predicate: u64, object: u128, subject: u128) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    let mut off = 0;
    off += encode_u64_be(predicate, &mut bytes[off..]);
    off += encode_u128_be(object, &mut bytes[off..]);
    off += encode_u128_be(subject, &mut bytes[off..]);
    CompositeKey { bytes, len: off as u8 }
}

/// Build an RDF OSP triple key.
pub fn rdf_osp_key(object: u128, subject: u128, predicate: u64) -> CompositeKey {
    let mut bytes = [0; MAX_KEY_LEN];
    let mut off = 0;
    off += encode_u128_be(object, &mut bytes[off..]);
    off += encode_u128_be(subject, &mut bytes[off..]);
    off += encode_u64_be(predicate, &mut bytes[off..]);
    CompositeKey { bytes, len: off as u8 }
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
    fn from_slice_truncates() {
        let long = vec![0xAB; MAX_KEY_LEN + 10];
        let k = CompositeKey::from_slice(&long);
        assert_eq!(k.len(), MAX_KEY_LEN);
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
