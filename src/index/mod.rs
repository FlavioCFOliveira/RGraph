//! B+ tree index core.
//!
//! Provides a persistent, latch-coupled B+ tree with optimistic reads and
//! bottom-up bulk loading.
//!
//! # Experimental, feature-gated modules
//!
//! Two prototype capabilities are gated behind off-by-default Cargo features
//! because they are not yet correctly wired into the tree:
//!
//! * `epoch` — the [`epoch::EpochPageTable`] lock-free page-snapshot table.
//!   It is unused by the tree and takes a mutex on every read, so it offers
//!   no lock-free benefit today.
//! * `prefix_compression` — per-page key prefix compression ([`prefix`]).
//!   The tree's comparators read raw record bytes, so enabling compression
//!   would corrupt ordering and lookups until every comparison path is made
//!   prefix-aware.

pub mod key;
pub mod page;
pub mod latch;
pub mod btree;
pub mod cursor;
pub mod bulk;
#[cfg(feature = "epoch")]
pub mod epoch;
pub mod value_codec;
#[cfg(feature = "prefix_compression")]
pub mod prefix;
pub mod property;
pub mod label;
pub mod adjacency_index;
pub mod rdf_store;
pub mod manager;
pub mod defrag;

pub use value_codec::{Value, encode, decode, encode_property_value, MAX_ENCODED_LEN};
#[cfg(feature = "prefix_compression")]
pub use prefix::{common_prefix, compress_record, decompress_record, extract_key};
pub mod type_index;

pub use key::{CompositeKey, KeyEncoder};
pub use page::{BTreePage, BTreePageType, BTREE_HEADER_SIZE};
pub use latch::{LatchCoupling, LatchMode};
pub use btree::{BPlusTree, BPlusTreeConfig};
pub use cursor::{BTreeCursor, BTreeRangeCursor, BTreeRangeScan};
pub use bulk::BulkLoader;
pub use label::LabelIndex;
pub use property::PropertyIndex;
pub use type_index::TypeIndex;
pub use adjacency_index::AdjacencyIndex;
pub use rdf_store::{RdfStore, RdfTriple, RdfQuad};
pub use manager::{IndexManager, IndexMutation};
