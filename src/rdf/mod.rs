//! RDF data model: terms, triples/quads, a persisted term dictionary, and a
//! crash-safe quad store.
//!
//! RGraph natively supports two graph data models — LPG and RDF
//! (see [`GraphMode`](crate::config::GraphMode)).  This module implements the
//! RDF side end-to-end:
//!
//! * [`term`] — [`Term`]s (IRIs, blank nodes, typed/lang literals) and their
//!   canonical, order-preserving byte encoding, plus a mapping to Cypher
//!   [`Value`](crate::cypher::value::Value)s for the common XSD datatypes.
//! * [`dictionary`] — a bidirectional [`TermDictionary`] interning each
//!   distinct term to a stable `u64` id, backed by persisted
//!   [`TermRecord`]s.
//! * [`triple`] — term-valued [`Triple`]/[`Quad`] shapes and the fixed-width
//!   id-valued [`TripleRecord`] on-disk format.
//! * [`store`] — [`RdfTripleStore`], the durable store that persists term and
//!   triple records to slotted data pages, WAL-logs every mutation, projects
//!   an SPO/POS/OSP permutation index for pattern matching, and rebuilds
//!   itself from the data pages on open.
//!
//! # On-disk record discrimination
//!
//! RDF primary records share slotted data pages with the engine's node/edge
//! records.  Node and edge records are recognised by their exact byte length
//! (32 and 64 respectively); RDF records carry a leading **magic byte** so the
//! index-rebuild scan can tell a term record from a triple record without
//! relying on length, and never mistakes an RDF record for a node/edge.

pub mod dictionary;
pub mod store;
pub mod term;
pub mod triple;

/// Leading discriminator byte of a persisted term-dictionary record.
///
/// Node/edge records are distinguished by exact length (32/64 bytes), but a
/// distinctive magic byte lets the rebuild scan and tooling identify RDF
/// records unambiguously and guards against any cross-kind aliasing.
pub const RDF_TERM_RECORD_MAGIC: u8 = 0xD1;

/// Leading discriminator byte of a persisted triple/quad record.
pub const RDF_TRIPLE_RECORD_MAGIC: u8 = 0xD2;

pub use dictionary::{TermDictionary, TermRecord};
pub use store::{RdfError, RdfTripleStore};
pub use term::{RdfLiteral, Term, xsd};
pub use triple::{Quad, Triple, TripleRecord};
