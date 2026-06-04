//! ID allocation and serialization primitives.
//!
//! [`Id`] is the primary key format for nodes, edges, and internal records.
//! It wraps a UUIDv7 so that identifiers are **monotonically increasing**
//! (K-sortable) and can be compared, hashed, and serialised efficiently.
//!
//! # Layout
//!
//! - 16 bytes on the wire (compact `u128` encoding).
//! - Lexicographic order == time order thanks to UUIDv7.

use crate::error::{RGraphError, Result};
use std::fmt;

/// A 16-byte database identifier backed by UUIDv7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(uuid::Uuid);

impl Id {
    /// Generate a new monotonically increasing identifier.
    ///
    /// Uses the system clock for the 48-bit Unix timestamp and a random
    /// 74-bit payload.  Successive calls on the same thread produce
    /// strictly increasing byte sequences (K-sortable).
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Reconstruct an [`Id`] from its raw 16-byte representation.
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::Argument`] if `bytes` is not a valid UUID.
    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self> {
        uuid::Uuid::from_slice(&bytes)
            .map(Self)
            .map_err(|e| RGraphError::Argument(format!("invalid id bytes: {}", e).into()))
    }

    /// Return the raw 16-byte representation.
    pub fn to_bytes(&self) -> [u8; 16] {
        *self.0.as_bytes()
    }

    /// Return the identifier as an unsigned 128-bit integer.
    ///
    /// This is useful for ordering and hashing in indexes.
    pub fn to_u128(&self) -> u128 {
        u128::from_be_bytes(self.to_bytes())
    }

    /// Encode the identifier into a compact byte buffer using [`bincode`].
    ///
    /// The wire format is a fixed 16-byte little-endian `u128`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self.to_u128())
            .map_err(|e| RGraphError::Argument(format!("id encode failed: {}", e).into()))
    }

    /// Decode an identifier from a compact byte buffer.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let val: u128 = bincode::deserialize(buf)
            .map_err(|e| RGraphError::Argument(format!("id decode failed: {}", e).into()))?;
        Self::from_bytes(val.to_be_bytes())
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn id_generation_produces_valid_uuid() {
        let id = Id::new();
        let bytes = id.to_bytes();
        assert_eq!(bytes.len(), 16);
        // Version 7 nibble should be 0x7.
        assert_eq!(bytes[6] >> 4, 0x7);
    }

    #[test]
    fn roundtrip_bytes() {
        let id = Id::new();
        let bytes = id.to_bytes();
        let recovered = Id::from_bytes(bytes).unwrap();
        assert_eq!(id, recovered);
    }

    #[test]
    fn roundtrip_u128() {
        let id = Id::new();
        let val = id.to_u128();
        let recovered = Id::from_bytes(val.to_be_bytes()).unwrap();
        assert_eq!(id, recovered);
    }

    #[test]
    fn roundtrip_bincode() {
        let id = Id::new();
        let encoded = id.encode().unwrap();
        let recovered = Id::decode(&encoded).unwrap();
        assert_eq!(id, recovered);
    }

    #[test]
    fn ids_are_monotonically_increasing() {
        let mut prev = Id::new();
        for _ in 0..1000 {
            let next = Id::new();
            assert!(
                prev.to_u128() < next.to_u128(),
                "expected monotonic increase: {} < {}",
                prev.to_u128(),
                next.to_u128()
            );
            prev = next;
        }
    }

    #[test]
    fn one_million_ids_no_collisions() {
        let mut seen = HashSet::with_capacity(1_000_000);
        for _ in 0..1_000_000 {
            let id = Id::new();
            let key = id.to_u128();
            assert!(
                seen.insert(key),
                "collision detected for id {}",
                id
            );
        }
        assert_eq!(seen.len(), 1_000_000);
    }

    #[test]
    fn invalid_bytes_rejected() {
        // A slice shorter than 16 bytes cannot be converted to Id.
        let short: &[u8] = &[0u8; 15];
        // from_bytes expects [u8; 16]; we test encode/decode instead.
        assert!(Id::decode(short).is_err());
    }

    #[test]
    fn display_is_uuid_string() {
        let id = Id::new();
        let s = id.to_string();
        assert_eq!(s.len(), 36); // standard UUID string length
    }
}
