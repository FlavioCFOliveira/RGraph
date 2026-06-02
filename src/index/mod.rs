//! B+ tree index core.
//!
//! Provides a persistent, latch-coupled B+ tree with optimistic reads,
//! crossbeam-epoch reclamation, and bottom-up bulk loading.

pub mod key;
pub mod page;
pub mod latch;
pub mod btree;
pub mod cursor;
pub mod bulk;
pub mod epoch;
pub mod value_codec;
pub mod prefix;
pub mod property;
pub mod label;
pub mod adjacency_index;
pub mod rdf_store;
pub mod manager;
pub mod defrag;

pub use value_codec::{Value, encode, decode, encode_property_value, MAX_ENCODED_LEN};
pub use prefix::{common_prefix, compress_record, decompress_record, extract_key};
pub mod type_index;

pub use key::{CompositeKey, KeyEncoder};
pub use page::{BTreePage, BTreePageType, BTREE_HEADER_SIZE};
pub use latch::{LatchCoupling, LatchMode};
pub use btree::{BPlusTree, BPlusTreeConfig};
pub use cursor::{BTreeCursor, BTreeRangeScan};
pub use bulk::BulkLoader;
pub use label::LabelIndex;
pub use property::PropertyIndex;
pub use type_index::TypeIndex;
pub use adjacency_index::AdjacencyIndex;
pub use rdf_store::{RdfStore, RdfTriple, RdfQuad};
pub use manager::{IndexManager, IndexMutation};
