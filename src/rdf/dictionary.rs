//! Term dictionary: bidirectional interning of RDF [`Term`]s to compact ids.
//!
//! Triples are stored as fixed-width `(s, p, o, g)` tuples of `u64` term ids
//! rather than variable-length terms, so a dictionary maps each distinct
//! term to a unique id and back.  The dictionary is the in-memory projection
//! of the persisted term records; it is rebuilt on open by replaying those
//! records (see [`crate::rdf::store::RdfTripleStore`]).
//!
//! # Id space
//!
//! Ids are `u64`, allocated monotonically from 1 (id 0 is the null sentinel,
//! matching the engine-wide convention).  Ids are never reused, so a term id
//! is stable for the lifetime of the database.

use crate::rdf::term::Term;
use std::collections::HashMap;

/// A single persisted dictionary entry: `(id, term)`.
///
/// On-disk layout (a primary record on a slotted data page):
///
/// ```text
/// 0x00  magic   u8   RDF_TERM_MAGIC (record-kind discriminator)
/// 0x01  id      u64  big-endian term id
/// 0x09  term    [..] canonical Term encoding (see Term::encode)
/// ```
///
/// The leading magic byte lets the index-rebuild scan distinguish a term
/// record from node (32-byte) / edge (64-byte) / triple records on the same
/// page without ambiguity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermRecord {
    /// The interned id.
    pub id: u64,
    /// The term bytes (canonical [`Term::encode`] form).
    pub term: Term,
}

impl TermRecord {
    /// Encode this record (magic byte + id + term bytes).
    pub fn encode(&self) -> Vec<u8> {
        let term_bytes = self.term.encode();
        let mut out = Vec::with_capacity(1 + 8 + term_bytes.len());
        out.push(super::RDF_TERM_RECORD_MAGIC);
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&term_bytes);
        out
    }

    /// Decode a record previously produced by [`TermRecord::encode`].
    ///
    /// Returns `None` if the magic byte does not match, the id is truncated,
    /// or the term bytes are malformed.
    pub fn decode(bytes: &[u8]) -> Option<TermRecord> {
        let (&magic, rest) = bytes.split_first()?;
        if magic != super::RDF_TERM_RECORD_MAGIC {
            return None;
        }
        let id_bytes = rest.get(0..8)?;
        let id = u64::from_be_bytes(id_bytes.try_into().ok()?);
        let term = Term::decode(rest.get(8..)?)?;
        Some(TermRecord { id, term })
    }
}

/// In-memory bidirectional term ↔ id map.
///
/// Not itself persisted: it is the cache rebuilt from [`TermRecord`]s on
/// open.  Lookups in both directions are O(1).
#[derive(Debug, Default)]
pub struct TermDictionary {
    term_to_id: HashMap<Term, u64>,
    id_to_term: HashMap<u64, Term>,
    next_id: u64,
}

impl TermDictionary {
    /// Create an empty dictionary; the first allocated id is 1.
    pub fn new() -> Self {
        Self {
            term_to_id: HashMap::new(),
            id_to_term: HashMap::new(),
            next_id: 1,
        }
    }

    /// Look up the id for `term`, if it is already interned.
    pub fn id_of(&self, term: &Term) -> Option<u64> {
        self.term_to_id.get(term).copied()
    }

    /// Look up the term for `id`, if present.
    pub fn term_of(&self, id: u64) -> Option<&Term> {
        self.id_to_term.get(&id)
    }

    /// Number of interned terms.
    pub fn len(&self) -> usize {
        self.term_to_id.len()
    }

    /// Is the dictionary empty?
    pub fn is_empty(&self) -> bool {
        self.term_to_id.is_empty()
    }

    /// The id the next freshly interned term would receive.
    pub fn peek_next_id(&self) -> u64 {
        self.next_id
    }

    /// Intern `term`, returning `(id, is_new)`.
    ///
    /// If the term is already present its existing id is returned with
    /// `is_new = false`; otherwise a fresh id is allocated and the term is
    /// inserted, returning `is_new = true`.  Callers persist a [`TermRecord`]
    /// exactly when `is_new` is true.
    pub fn intern(&mut self, term: Term) -> (u64, bool) {
        if let Some(&id) = self.term_to_id.get(&term) {
            return (id, false);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.term_to_id.insert(term.clone(), id);
        self.id_to_term.insert(id, term);
        (id, true)
    }

    /// Re-insert a `(id, term)` pair recovered from disk during rebuild.
    ///
    /// Unlike [`intern`](Self::intern) this does not allocate a fresh id: it
    /// trusts the persisted id and advances `next_id` past it so future
    /// allocations never collide with recovered ids.  Idempotent: replaying
    /// the same record twice is a no-op.
    pub fn reinsert(&mut self, id: u64, term: Term) {
        self.term_to_id.insert(term.clone(), id);
        self.id_to_term.insert(id, term);
        if id >= self.next_id {
            self.next_id = id + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::term::{RdfLiteral, xsd};

    #[test]
    fn intern_assigns_increasing_ids() {
        let mut dict = TermDictionary::new();
        let (a, new_a) = dict.intern(Term::iri("http://x/a"));
        let (b, new_b) = dict.intern(Term::iri("http://x/b"));
        assert_eq!((a, new_a), (1, true));
        assert_eq!((b, new_b), (2, true));
    }

    #[test]
    fn intern_is_idempotent() {
        let mut dict = TermDictionary::new();
        let (a1, new1) = dict.intern(Term::iri("http://x/a"));
        let (a2, new2) = dict.intern(Term::iri("http://x/a"));
        assert_eq!(a1, a2);
        assert!(new1);
        assert!(!new2);
        assert_eq!(dict.len(), 1);
    }

    #[test]
    fn bidirectional_lookup() {
        let mut dict = TermDictionary::new();
        let t = Term::literal(RdfLiteral::typed("42", xsd::INTEGER));
        let (id, _) = dict.intern(t.clone());
        assert_eq!(dict.id_of(&t), Some(id));
        assert_eq!(dict.term_of(id), Some(&t));
    }

    #[test]
    fn term_record_roundtrip() {
        let rec = TermRecord {
            id: 7,
            term: Term::iri("http://example.org/thing"),
        };
        let bytes = rec.encode();
        assert_eq!(TermRecord::decode(&bytes), Some(rec));
    }

    #[test]
    fn term_record_decode_rejects_foreign_magic() {
        // A 32-byte buffer (the size of a NodeRecord) must not decode as a
        // term record — the magic byte guards against cross-kind aliasing.
        let buf = [0u8; 32];
        assert_eq!(TermRecord::decode(&buf), None);
    }

    #[test]
    fn reinsert_advances_next_id() {
        let mut dict = TermDictionary::new();
        dict.reinsert(5, Term::iri("http://x/a"));
        // The next freshly interned id must be > 5.
        let (id, _) = dict.intern(Term::iri("http://x/b"));
        assert_eq!(id, 6);
    }

    #[test]
    fn reinsert_is_idempotent() {
        let mut dict = TermDictionary::new();
        dict.reinsert(1, Term::iri("http://x/a"));
        dict.reinsert(1, Term::iri("http://x/a"));
        assert_eq!(dict.len(), 1);
        assert_eq!(dict.id_of(&Term::iri("http://x/a")), Some(1));
    }
}
