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

pub use key::{CompositeKey, KeyEncoder};
pub use page::{BTreePage, BTreePageType, BTREE_HEADER_SIZE};
pub use latch::{LatchCoupling, LatchMode};
pub use btree::{BPlusTree, BPlusTreeConfig};
pub use cursor::{BTreeCursor, BTreeRangeScan};
pub use bulk::BulkLoader;
