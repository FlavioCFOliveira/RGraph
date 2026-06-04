//! RDF triple and quad store with three composite-key B+ tree indexes.
//!
//! Supports exact-match and partial-key lookups for SPARQL-style
//! pattern matching:
//! - SPO (subject + predicate + object)
//! - POS (predicate + object + subject)
//! - OSP (object + subject + predicate)
//!
//! Quads add a `graph_id` field that is stored in the value payload
//! alongside the [`SlotRef`] so that graph-context lookups are possible
//! without a fourth index.

use crate::graph::record::SlotRef;
use crate::index::btree::{BPlusTree, BPlusTreeConfig, BTreeError};
use crate::index::key::{rdf_osp_key, rdf_pos_key, rdf_spo_key};
use crate::index::page::BTreePage;

/// A single RDF triple (subject, predicate, object).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdfTriple {
    pub subject: u128,
    pub predicate: u64,
    pub object: u128,
}

/// An RDF quad (graph, subject, predicate, object).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdfQuad {
    pub graph: u64,
    pub triple: RdfTriple,
}

impl RdfQuad {
    pub fn new(graph: u64, subject: u128, predicate: u64, object: u128) -> Self {
        Self {
            graph,
            triple: RdfTriple {
                subject,
                predicate,
                object,
            },
        }
    }
}

/// The three RDF index permutations.  Each stores the same triples under a
/// different field ordering so that any partial pattern can be answered by a
/// prefix scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permutation {
    /// `subject (16) | predicate (8) | object (16)`.
    Spo,
    /// `predicate (8) | object (16) | subject (16)`.
    Pos,
    /// `object (16) | subject (16) | predicate (8)`.
    Osp,
}

impl Permutation {
    /// De-permute a 40-byte index key back into a canonical [`RdfTriple`].
    ///
    /// Returns `None` if the key is too short.
    fn decode_triple(self, key: &[u8]) -> Option<RdfTriple> {
        if key.len() < 40 {
            return None;
        }
        use crate::index::key::{decode_u128_be, decode_u64_be};
        let (subject, predicate, object) = match self {
            Permutation::Spo => (
                decode_u128_be(&key[0..16]),
                decode_u64_be(&key[16..24]),
                decode_u128_be(&key[24..40]),
            ),
            Permutation::Pos => {
                let predicate = decode_u64_be(&key[0..8]);
                let object = decode_u128_be(&key[8..24]);
                let subject = decode_u128_be(&key[24..40]);
                (subject, predicate, object)
            }
            Permutation::Osp => {
                let object = decode_u128_be(&key[0..16]);
                let subject = decode_u128_be(&key[16..32]);
                let predicate = decode_u64_be(&key[32..40]);
                (subject, predicate, object)
            }
        };
        Some(RdfTriple {
            subject,
            predicate,
            object,
        })
    }
}

/// RDF store backed by three B+ tree indexes.
#[derive(Debug)]
pub struct RdfStore {
    spo: BPlusTree,
    pos: BPlusTree,
    osp: BPlusTree,
}

impl Default for RdfStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RdfStore {
    /// Create a new empty RDF store.
    pub fn new() -> Self {
        let config = BPlusTreeConfig::default();
        Self {
            spo: BPlusTree::new(config.clone()),
            pos: BPlusTree::new(config.clone()),
            osp: BPlusTree::new(config),
        }
    }

    /// Insert a triple (and optional graph context) into all three indexes.
    pub fn insert(
        &self,
        triple: &RdfTriple,
        slot: SlotRef,
        graph: Option<u64>,
    ) -> Result<(), BTreeError> {
        let value = encode_value(slot, graph);
        self.spo.insert(
            &rdf_spo_key(triple.subject, triple.predicate, triple.object),
            &value,
        )?;
        self.pos.insert(
            &rdf_pos_key(triple.predicate, triple.object, triple.subject),
            &value,
        )?;
        self.osp.insert(
            &rdf_osp_key(triple.object, triple.subject, triple.predicate),
            &value,
        )?;
        Ok(())
    }

    /// Delete a triple from all three indexes. Returns `true` if the triple
    /// existed in at least one index.
    pub fn delete(
        &self,
        triple: &RdfTriple,
    ) -> Result<bool, BTreeError> {
        let spo_key = rdf_spo_key(triple.subject, triple.predicate, triple.object);
        let found = self.spo.delete(&spo_key)?;
        let pos_key = rdf_pos_key(triple.predicate, triple.object, triple.subject);
        self.pos.delete(&pos_key)?;
        let osp_key = rdf_osp_key(triple.object, triple.subject, triple.predicate);
        self.osp.delete(&osp_key)?;
        Ok(found)
    }

    /// Look up an exact triple in the SPO index.
    pub fn lookup(&self, triple: &RdfTriple) -> Option<(SlotRef, Option<u64>)> {
        let key = rdf_spo_key(triple.subject, triple.predicate, triple.object);
        let (pid, slot) = self.spo.search(&key)?;
        let page = self.spo.get_page(pid)?;
        decode_value(&page, slot)
    }

    /// Scan triples matching `(subject, predicate, ?object)`.
    pub fn scan_by_subject_predicate(
        &self,
        subject: u128,
        predicate: u64,
    ) -> Vec<(RdfTriple, SlotRef, Option<u64>)> {
        self.scan_prefix(
            &self.spo,
            Permutation::Spo,
            &rdf_spo_key(subject, predicate, 0).as_slice()[..24],
        )
    }

    /// Scan triples matching `(subject, ?predicate, ?object)`.
    pub fn scan_by_subject(&self, subject: u128) -> Vec<(RdfTriple, SlotRef, Option<u64>)> {
        self.scan_prefix(
            &self.spo,
            Permutation::Spo,
            &rdf_spo_key(subject, 0, 0).as_slice()[..16],
        )
    }

    /// Scan triples matching `(?subject, predicate, object)`.
    pub fn scan_by_predicate_object(
        &self,
        predicate: u64,
        object: u128,
    ) -> Vec<(RdfTriple, SlotRef, Option<u64>)> {
        self.scan_prefix(
            &self.pos,
            Permutation::Pos,
            &rdf_pos_key(predicate, object, 0).as_slice()[..24],
        )
    }

    /// Scan triples matching `(?subject, ?predicate, object)`.
    pub fn scan_by_object(&self, object: u128) -> Vec<(RdfTriple, SlotRef, Option<u64>)> {
        self.scan_prefix(
            &self.osp,
            Permutation::Osp,
            &rdf_osp_key(object, 0, 0).as_slice()[..16],
        )
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Generic prefix scan over one of the three indexes.
    ///
    /// `perm` identifies the index ordering so that each matched key is
    /// de-permuted back to a canonical triple — without it, POS/OSP scans would
    /// return triples with scrambled subject/predicate/object fields.
    fn scan_prefix(
        &self,
        tree: &BPlusTree,
        perm: Permutation,
        prefix: &[u8],
    ) -> Vec<(RdfTriple, SlotRef, Option<u64>)> {
        let mut results = Vec::new();
        let root_id = tree.root_page_id.load(std::sync::atomic::Ordering::Relaxed);
        if tree.get_page(root_id).is_none() {
            return results;
        }

        // Navigate to the left-most leaf that could contain the prefix.
        let leaf_id = find_leaf(tree, root_id, prefix);
        let mut leaf = match tree.get_page(leaf_id) {
            Some(p) => p,
            None => return results,
        };

        loop {
            for i in 0..leaf.key_count() {
                if let Some(kv) = leaf.key(i) {
                    let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
                    let key = &kv[2..2 + key_len];
                    if !key.starts_with(prefix) {
                        continue;
                    }
                    if let Some((triple, slot, graph)) =
                        decode_key_and_value(perm, key, &kv[2 + key_len..])
                    {
                        results.push((triple, slot, graph));
                    }
                }
            }
            let next = leaf.btree_header().sibling_next;
            if next == 0 {
                break;
            }
            leaf = match tree.get_page(next) {
                Some(p) => p,
                None => break,
            };
        }

        results
    }
}

/// Encode SlotRef + optional graph into a fixed-size value payload.
fn encode_value(slot: SlotRef, graph: Option<u64>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12);
    buf.extend_from_slice(&slot.raw.to_be_bytes());
    if let Some(g) = graph {
        buf.extend_from_slice(&g.to_be_bytes());
    }
    buf
}

/// Decode a value payload into (SlotRef, optional graph).
fn decode_value(page: &BTreePage, slot: u16) -> Option<(SlotRef, Option<u64>)> {
    let kv = page.key(slot)?;
    let key_len = u16::from_be_bytes([kv[0], kv[1]]) as usize;
    let value = &kv[2 + key_len..];
    if value.len() < 4 {
        return None;
    }
    let raw = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    let graph = if value.len() >= 12 {
        Some(u64::from_be_bytes([
            value[4], value[5], value[6], value[7],
            value[8], value[9], value[10], value[11],
        ]))
    } else {
        None
    };
    Some((SlotRef { raw }, graph))
}

/// Reconstruct a triple and metadata from a raw index key + value.
///
/// `perm` selects how the 40-byte key is de-permuted back into canonical
/// `(subject, predicate, object)` order.
fn decode_key_and_value(
    perm: Permutation,
    key: &[u8],
    value: &[u8],
) -> Option<(RdfTriple, SlotRef, Option<u64>)> {
    let triple = perm.decode_triple(key)?;
    if value.len() < 4 {
        return None;
    }
    let raw = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    let graph = if value.len() >= 12 {
        Some(u64::from_be_bytes([
            value[4], value[5], value[6], value[7],
            value[8], value[9], value[10], value[11],
        ]))
    } else {
        None
    };
    Some((triple, SlotRef { raw }, graph))
}

/// Find the leaf page that should contain keys starting with `prefix`.
fn find_leaf(tree: &BPlusTree, mut page_id: u64, prefix: &[u8]) -> u64 {
    loop {
        let page = match tree.get_page(page_id) {
            Some(p) => p,
            None => return page_id,
        };
        if page.is_leaf() {
            return page_id;
        }
        page_id = branch_child(&page, prefix);
    }
}

/// Given a branch page and a prefix, return the child page id to follow.
fn branch_child(page: &BTreePage, prefix: &[u8]) -> u64 {
    let count = page.key_count();
    let mut lo = 0usize;
    let mut hi = count as usize;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let sep = page.separator_key(mid as u16);
        if let Some(sep) = sep {
            if sep < prefix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        } else {
            hi = mid;
        }
    }
    if lo < count as usize {
        page.child_pointer(lo as u16).unwrap_or(0)
    } else {
        page.btree_header().rightmost_child
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_lookup_exact() {
        let store = RdfStore::new();
        let triple = RdfTriple {
            subject: 1,
            predicate: 10,
            object: 100,
        };
        let slot = SlotRef::new(5, 3);
        store.insert(&triple, slot, Some(7)).unwrap();
        let result = store.lookup(&triple);
        assert_eq!(result, Some((slot, Some(7))));
    }

    #[test]
    fn delete_removes_from_all_indexes() {
        let store = RdfStore::new();
        let t = RdfTriple {
            subject: 1,
            predicate: 10,
            object: 100,
        };
        store.insert(&t, SlotRef::new(1, 0), None).unwrap();
        assert!(store.delete(&t).unwrap());
        assert!(store.lookup(&t).is_none());
    }

    #[test]
    fn scan_by_subject_predicate() {
        let store = RdfStore::new();
        let t1 = RdfTriple {
            subject: 1,
            predicate: 10,
            object: 100,
        };
        let t2 = RdfTriple {
            subject: 1,
            predicate: 10,
            object: 101,
        };
        let t3 = RdfTriple {
            subject: 1,
            predicate: 11,
            object: 200,
        };
        store.insert(&t1, SlotRef::new(1, 0), None).unwrap();
        store.insert(&t2, SlotRef::new(2, 0), None).unwrap();
        store.insert(&t3, SlotRef::new(3, 0), None).unwrap();

        let results = store.scan_by_subject_predicate(1, 10);
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|(t, _, _)| t.object == 100));
        assert!(results.iter().any(|(t, _, _)| t.object == 101));
    }

    #[test]
    fn scan_by_subject() {
        let store = RdfStore::new();
        for i in 1u128..=50 {
            let t = RdfTriple {
                subject: 42,
                predicate: i as u64,
                object: i * 10,
            };
            store.insert(&t, SlotRef::new(i as u32, 0), None).unwrap();
        }
        let results = store.scan_by_subject(42);
        assert_eq!(results.len(), 50);
    }

    #[test]
    fn scan_by_object() {
        let store = RdfStore::new();
        let t1 = RdfTriple {
            subject: 1,
            predicate: 10,
            object: 99,
        };
        let t2 = RdfTriple {
            subject: 2,
            predicate: 20,
            object: 99,
        };
        let t3 = RdfTriple {
            subject: 3,
            predicate: 30,
            object: 100,
        };
        store.insert(&t1, SlotRef::new(1, 0), None).unwrap();
        store.insert(&t2, SlotRef::new(2, 0), None).unwrap();
        store.insert(&t3, SlotRef::new(3, 0), None).unwrap();

        let results = store.scan_by_object(99);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn scan_by_predicate_object() {
        let store = RdfStore::new();
        let t1 = RdfTriple {
            subject: 1,
            predicate: 10,
            object: 99,
        };
        let t2 = RdfTriple {
            subject: 2,
            predicate: 10,
            object: 99,
        };
        let t3 = RdfTriple {
            subject: 3,
            predicate: 20,
            object: 99,
        };
        store.insert(&t1, SlotRef::new(1, 0), None).unwrap();
        store.insert(&t2, SlotRef::new(2, 0), None).unwrap();
        store.insert(&t3, SlotRef::new(3, 0), None).unwrap();

        let results = store.scan_by_predicate_object(10, 99);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn pos_scan_returns_correctly_depermuted_triples() {
        // A POS scan reads keys laid out as predicate|object|subject; without
        // de-permutation the returned triple fields would be scrambled.
        let store = RdfStore::new();
        let t1 = RdfTriple { subject: 11, predicate: 7, object: 99 };
        let t2 = RdfTriple { subject: 22, predicate: 7, object: 99 };
        store.insert(&t1, SlotRef::new(1, 0), None).unwrap();
        store.insert(&t2, SlotRef::new(2, 0), None).unwrap();

        let results = store.scan_by_predicate_object(7, 99);
        assert_eq!(results.len(), 2);
        let mut subjects: Vec<u128> = results.iter().map(|(t, _, _)| t.subject).collect();
        subjects.sort_unstable();
        assert_eq!(subjects, vec![11, 22], "subjects must be de-permuted correctly");
        for (t, _, _) in &results {
            assert_eq!(t.predicate, 7, "predicate must round-trip");
            assert_eq!(t.object, 99, "object must round-trip");
        }
    }

    #[test]
    fn osp_scan_returns_correctly_depermuted_triples() {
        // An OSP scan reads keys laid out as object|subject|predicate.
        let store = RdfStore::new();
        let t1 = RdfTriple { subject: 5, predicate: 30, object: 100 };
        let t2 = RdfTriple { subject: 6, predicate: 40, object: 100 };
        store.insert(&t1, SlotRef::new(1, 0), None).unwrap();
        store.insert(&t2, SlotRef::new(2, 0), None).unwrap();

        let results = store.scan_by_object(100);
        assert_eq!(results.len(), 2);
        assert!(
            results.iter().any(|(t, _, _)| *t == t1),
            "OSP scan must reconstruct {t1:?} exactly"
        );
        assert!(
            results.iter().any(|(t, _, _)| *t == t2),
            "OSP scan must reconstruct {t2:?} exactly"
        );
    }

    #[test]
    fn insert_many_and_scan() {
        let store = RdfStore::new();
        for s in 1u128..=10 {
            for p in 1u64..=10 {
                for o in 1u128..=10 {
                    let t = RdfTriple { subject: s, predicate: p, object: o };
                    store.insert(&t, SlotRef::new((s + p as u128 + o) as u32, 0), None).unwrap();
                }
            }
        }
        let results = store.scan_by_subject_predicate(5, 5);
        assert_eq!(results.len(), 10);
    }
}
