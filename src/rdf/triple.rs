//! RDF triple and quad records over interned term ids.
//!
//! A [`Triple`] / [`Quad`] is the in-memory, *term-valued* shape produced by
//! the parser and returned by pattern matching.  A [`TripleRecord`] is the
//! fixed-width, *id-valued* on-disk form: four `u64` term ids
//! `(subject, predicate, object, graph)`, where `graph == 0` denotes the
//! default (unnamed) graph.
//!
//! Storing ids rather than terms keeps each triple record a compact 33 bytes
//! and lets the permutation indexes ([`crate::index::rdf_store::RdfStore`])
//! key on fixed-width integers.

use crate::rdf::term::Term;

/// A term-valued RDF triple.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Triple {
    /// Subject (IRI or blank node).
    pub subject: Term,
    /// Predicate (always an IRI in well-formed RDF; stored as a [`Term`] for
    /// uniformity).
    pub predicate: Term,
    /// Object (IRI, blank node, or literal).
    pub object: Term,
}

impl Triple {
    /// Construct a triple.
    pub fn new(subject: Term, predicate: Term, object: Term) -> Self {
        Self {
            subject,
            predicate,
            object,
        }
    }
}

/// A term-valued RDF quad: a triple plus an optional named graph.
///
/// `graph == None` is the default graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Quad {
    /// The triple.
    pub triple: Triple,
    /// The named graph, or `None` for the default graph.
    pub graph: Option<Term>,
}

impl Quad {
    /// Construct a quad in the default graph.
    pub fn triple(triple: Triple) -> Self {
        Self {
            triple,
            graph: None,
        }
    }

    /// Construct a quad in a named graph.
    pub fn in_graph(triple: Triple, graph: Term) -> Self {
        Self {
            triple,
            graph: Some(graph),
        }
    }
}

/// A fixed-width, id-valued triple/quad record (33 bytes).
///
/// On-disk layout (a primary record on a slotted data page):
///
/// ```text
/// 0x00  magic      u8   RDF_TRIPLE_RECORD_MAGIC
/// 0x01  subject    u64  big-endian subject term id
/// 0x09  predicate  u64  big-endian predicate term id
/// 0x11  object     u64  big-endian object term id
/// 0x19  graph      u64  big-endian graph term id (0 = default graph)
/// ```
///
/// The leading magic byte lets the index-rebuild scan distinguish a triple
/// record from node (32-byte) / edge (64-byte) / term records sharing a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TripleRecord {
    /// Subject term id (non-zero).
    pub subject: u64,
    /// Predicate term id (non-zero).
    pub predicate: u64,
    /// Object term id (non-zero).
    pub object: u64,
    /// Graph term id; `0` denotes the default graph.
    pub graph: u64,
}

impl TripleRecord {
    /// On-disk size in bytes (magic + four u64 ids).
    pub const SIZE: usize = 1 + 8 * 4;

    /// Construct a record from its four term ids.
    pub fn new(subject: u64, predicate: u64, object: u64, graph: u64) -> Self {
        Self {
            subject,
            predicate,
            object,
            graph,
        }
    }

    /// Is this record in the default (unnamed) graph?
    pub fn is_default_graph(&self) -> bool {
        self.graph == 0
    }

    /// Encode this record into a fixed-size byte vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        out.push(super::RDF_TRIPLE_RECORD_MAGIC);
        out.extend_from_slice(&self.subject.to_be_bytes());
        out.extend_from_slice(&self.predicate.to_be_bytes());
        out.extend_from_slice(&self.object.to_be_bytes());
        out.extend_from_slice(&self.graph.to_be_bytes());
        out
    }

    /// Decode a record previously produced by [`TripleRecord::encode`].
    ///
    /// Returns `None` if the length is wrong or the magic byte does not match.
    pub fn decode(bytes: &[u8]) -> Option<TripleRecord> {
        if bytes.len() != Self::SIZE {
            return None;
        }
        if bytes[0] != super::RDF_TRIPLE_RECORD_MAGIC {
            return None;
        }
        let subject = u64::from_be_bytes(bytes[1..9].try_into().ok()?);
        let predicate = u64::from_be_bytes(bytes[9..17].try_into().ok()?);
        let object = u64::from_be_bytes(bytes[17..25].try_into().ok()?);
        let graph = u64::from_be_bytes(bytes[25..33].try_into().ok()?);
        Some(TripleRecord {
            subject,
            predicate,
            object,
            graph,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_record_size_is_33() {
        assert_eq!(TripleRecord::SIZE, 33);
    }

    #[test]
    fn triple_record_roundtrip() {
        let rec = TripleRecord::new(1, 2, 3, 4);
        let bytes = rec.encode();
        assert_eq!(bytes.len(), TripleRecord::SIZE);
        assert_eq!(TripleRecord::decode(&bytes), Some(rec));
    }

    #[test]
    fn default_graph_roundtrip() {
        let rec = TripleRecord::new(10, 20, 30, 0);
        assert!(rec.is_default_graph());
        let bytes = rec.encode();
        assert_eq!(TripleRecord::decode(&bytes), Some(rec));
    }

    #[test]
    fn decode_rejects_wrong_size() {
        assert!(TripleRecord::decode(&[0u8; 32]).is_none());
        assert!(TripleRecord::decode(&[0u8; 34]).is_none());
    }

    #[test]
    fn decode_rejects_foreign_magic() {
        let mut bytes = TripleRecord::new(1, 2, 3, 4).encode();
        bytes[0] = 0x00;
        assert!(TripleRecord::decode(&bytes).is_none());
    }
}
