//! Persistent RDF triple/quad store.
//!
//! [`RdfTripleStore`] is the durable, crash-safe home of RDF data.  It ties
//! together three pieces:
//!
//! * a [`TermDictionary`] that interns each distinct [`Term`] to a `u64` id;
//! * persisted primary records — [`TermRecord`]s and [`TripleRecord`]s —
//!   written to slotted data pages and WAL-logged so they survive a crash;
//! * an in-memory [`RdfStore`] (SPO/POS/OSP permutation indexes) projected
//!   over the persisted triples for efficient pattern matching.
//!
//! # Durability model
//!
//! The store mirrors the engine's node/edge discipline exactly:
//!
//! 1. The primary record (term or triple) is written to a slotted data page
//!    via the page manager and the page is flushed.
//! 2. A WAL record ([`RecordType::RdfTripleInsert`] /
//!    [`RecordType::RdfTripleDelete`]) is appended so recovery can attribute
//!    the change.  These are *logical* records (like `NodeInsert`): physical
//!    REDO skips them and the store is rebuilt from the data pages on open.
//! 3. The in-memory dictionary and permutation index are updated.
//!
//! On open the persisted records are replayed by [`RdfTripleStore::rebuild`],
//! which rebuilds the dictionary and the permutation index from the data
//! pages — so the in-memory structures never need to be persisted directly.
//!
//! # Term-id width
//!
//! The permutation index ([`RdfStore`]) keys on `u128` subjects/objects and a
//! `u64` predicate.  Term ids are `u64`; subjects and objects are widened to
//! `u128` losslessly, predicates stay `u64`, and the graph id is carried in
//! the index value payload.  De-widening on the way out is exact.

use crate::graph::record::SlotRef;
use crate::index::rdf_store::{RdfStore, RdfTriple};
use crate::io::{AlignedBuffer, FileSystem};
use crate::rdf::dictionary::{TermDictionary, TermRecord};
use crate::rdf::term::Term;
use crate::rdf::triple::{Quad, Triple, TripleRecord};
use crate::storage::manager::PageManager;
use crate::storage::page::{PAGE_SIZE, PageId, PageType, SlottedPage};
use crate::wal::record::{RecordType, WalRecord};
use crate::wal::writer::WalWriter;
use std::collections::HashMap;

/// Errors surfaced by the RDF triple store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdfError {
    /// An underlying page or WAL I/O operation failed.
    Io,
    /// A slotted page had no free space and a fresh page could not hold the
    /// record (should never happen for fixed-size RDF records).
    PageFull,
    /// The permutation index rejected the mutation.
    Index,
}

impl std::fmt::Display for RdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RdfError::Io => write!(f, "RDF store I/O error"),
            RdfError::PageFull => write!(f, "RDF data page full"),
            RdfError::Index => write!(f, "RDF index error"),
        }
    }
}

impl std::error::Error for RdfError {}

/// A pattern position: either a fixed term id or a wildcard.
type Pos = Option<u64>;

/// The persistent RDF triple/quad store.
#[derive(Debug)]
pub struct RdfTripleStore {
    /// Term ↔ id interning map (rebuilt on open).
    dictionary: TermDictionary,
    /// SPO/POS/OSP permutation index over `(s_id, p_id, o_id)` with graph in
    /// the value payload (rebuilt on open).
    index: RdfStore,
    /// Slotted data pages owned by the RDF store, in allocation order.
    rdf_pages: Vec<PageId>,
    /// In-memory mirror of every live triple record, keyed by `(s,p,o,g)`, so
    /// duplicate inserts are idempotent and pattern scans over wildcards have
    /// a fast fallback that does not depend on permutation de-keying.
    triples: HashMap<(u64, u64, u64, u64), SlotRef>,
}

impl Default for RdfTripleStore {
    fn default() -> Self {
        Self::new()
    }
}

/// A quad whose term/triple records have been built into a pending page-image
/// set by [`RdfTripleStore::prepare_quad`], awaiting a durable commit before the
/// in-memory permutation index is updated (no-steal RDF inserts, finding C1).
pub struct PreparedQuad {
    key: (u64, u64, u64, u64),
    triple_slot: SlotRef,
}

impl RdfTripleStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            dictionary: TermDictionary::new(),
            index: RdfStore::new(),
            rdf_pages: Vec::new(),
            triples: HashMap::new(),
        }
    }

    /// Number of distinct live triples currently stored.
    pub fn triple_count(&self) -> usize {
        self.triples.len()
    }

    /// Number of interned terms.
    pub fn term_count(&self) -> usize {
        self.dictionary.len()
    }

    /// Borrow the term dictionary (read-only).
    pub fn dictionary(&self) -> &TermDictionary {
        &self.dictionary
    }

    // ------------------------------------------------------------------
    // Mutation
    // ------------------------------------------------------------------

    /// Insert a triple in the default graph.  See [`insert_quad`].
    ///
    /// [`insert_quad`]: RdfTripleStore::insert_quad
    pub fn insert_triple(
        &mut self,
        pm: &mut PageManager,
        wal: &mut WalWriter,
        triple: &Triple,
        fs: &dyn FileSystem,
    ) -> Result<bool, RdfError> {
        self.insert_quad(pm, wal, &Quad::triple(triple.clone()), fs)
    }

    /// Insert a quad, interning any new terms and persisting the triple
    /// record.  Returns `true` if the quad was newly added, `false` if it was
    /// already present (idempotent).
    ///
    /// # Errors
    ///
    /// Returns [`RdfError`] on page or WAL I/O failure.
    pub fn insert_quad(
        &mut self,
        pm: &mut PageManager,
        wal: &mut WalWriter,
        quad: &Quad,
        fs: &dyn FileSystem,
    ) -> Result<bool, RdfError> {
        let s_id = self.intern(pm, wal, &quad.triple.subject, fs)?;
        let p_id = self.intern(pm, wal, &quad.triple.predicate, fs)?;
        let o_id = self.intern(pm, wal, &quad.triple.object, fs)?;
        let g_id = match &quad.graph {
            Some(g) => self.intern(pm, wal, g, fs)?,
            None => 0,
        };

        let key = (s_id, p_id, o_id, g_id);
        if self.triples.contains_key(&key) {
            return Ok(false);
        }

        let record = TripleRecord::new(s_id, p_id, o_id, g_id);
        let slot = self.persist_record(pm, wal, &record.encode(), fs)?;

        self.index_insert(s_id, p_id, o_id, g_id, slot)?;
        self.triples.insert(key, slot);
        Ok(true)
    }

    /// Delete a triple in the default graph.  Returns `true` if it existed.
    pub fn delete_triple(
        &mut self,
        pm: &mut PageManager,
        wal: &mut WalWriter,
        triple: &Triple,
        fs: &dyn FileSystem,
    ) -> Result<bool, RdfError> {
        self.delete_quad(pm, wal, &Quad::triple(triple.clone()), fs)
    }

    /// Delete a quad.  Returns `true` if it existed and was removed.
    ///
    /// The on-disk record is tombstoned (its slot is removed from the page)
    /// and the deletion is WAL-logged.  Interned terms are *not* reclaimed —
    /// term ids are stable for the database's lifetime.
    pub fn delete_quad(
        &mut self,
        pm: &mut PageManager,
        wal: &mut WalWriter,
        quad: &Quad,
        fs: &dyn FileSystem,
    ) -> Result<bool, RdfError> {
        let Some(s_id) = self.dictionary.id_of(&quad.triple.subject) else {
            return Ok(false);
        };
        let Some(p_id) = self.dictionary.id_of(&quad.triple.predicate) else {
            return Ok(false);
        };
        let Some(o_id) = self.dictionary.id_of(&quad.triple.object) else {
            return Ok(false);
        };
        let g_id = match &quad.graph {
            Some(g) => match self.dictionary.id_of(g) {
                Some(id) => id,
                None => return Ok(false),
            },
            None => 0,
        };

        let key = (s_id, p_id, o_id, g_id);
        let Some(slot) = self.triples.remove(&key) else {
            return Ok(false);
        };

        // Tombstone the on-disk record (best-effort) and WAL-log the delete.
        self.tombstone_record(pm, slot, fs)?;
        let payload = TripleRecord::new(s_id, p_id, o_id, g_id).encode();
        Self::log(pm, wal, fs, RecordType::RdfTripleDelete, payload)?;

        // Remove from the permutation index.  The graph dimension lives in the
        // value payload, so removing the (s,p,o) key is sufficient when there
        // is a single (s,p,o,g); for multi-graph (s,p,o) we re-index below.
        self.index
            .delete(&RdfTriple {
                subject: s_id as u128,
                predicate: p_id,
                object: o_id as u128,
            })
            .map_err(|_| RdfError::Index)?;
        self.reindex_spo(s_id, p_id, o_id);
        Ok(true)
    }

    // ------------------------------------------------------------------
    // Pattern matching
    // ------------------------------------------------------------------

    /// Match a quad pattern, returning the matching quads as term-valued
    /// [`Quad`]s.  Any of `subject`/`predicate`/`object`/`graph` may be
    /// `None` (a wildcard).  A `graph` of `Some(None)` is not expressible
    /// here — pass `graph = None` to match all graphs and filter by the
    /// returned `Quad::graph` if a specific graph is required, or use
    /// [`match_default_graph`].
    ///
    /// [`match_default_graph`]: RdfTripleStore::match_triples
    pub fn match_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&Term>,
        object: Option<&Term>,
        graph: Option<&Term>,
    ) -> Vec<Quad> {
        // Resolve each bound position to an id.  If any bound term is unknown
        // to the dictionary, no triple can match it.
        let s = match self.resolve(subject) {
            Resolved::Wildcard => None,
            Resolved::Id(id) => Some(id),
            Resolved::Unknown => return Vec::new(),
        };
        let p = match self.resolve(predicate) {
            Resolved::Wildcard => None,
            Resolved::Id(id) => Some(id),
            Resolved::Unknown => return Vec::new(),
        };
        let o = match self.resolve(object) {
            Resolved::Wildcard => None,
            Resolved::Id(id) => Some(id),
            Resolved::Unknown => return Vec::new(),
        };
        let g = match self.resolve(graph) {
            Resolved::Wildcard => None,
            Resolved::Id(id) => Some(id),
            Resolved::Unknown => return Vec::new(),
        };

        self.match_ids(s, p, o, g)
            .into_iter()
            .filter_map(|(s, p, o, gph)| self.materialise(s, p, o, gph))
            .collect()
    }

    /// Match a triple pattern, ignoring the graph dimension (matches across
    /// all graphs, default included).  Returns **distinct** term-valued
    /// [`Triple`]s: a triple asserted in several graphs is returned once.  Use
    /// [`match_pattern`](Self::match_pattern) when the graph dimension matters.
    pub fn match_triples(
        &self,
        subject: Option<&Term>,
        predicate: Option<&Term>,
        object: Option<&Term>,
    ) -> Vec<Triple> {
        let mut seen: std::collections::HashSet<(u64, u64, u64)> = std::collections::HashSet::new();
        let s = match self.resolve(subject) {
            Resolved::Wildcard => None,
            Resolved::Id(id) => Some(id),
            Resolved::Unknown => return Vec::new(),
        };
        let p = match self.resolve(predicate) {
            Resolved::Wildcard => None,
            Resolved::Id(id) => Some(id),
            Resolved::Unknown => return Vec::new(),
        };
        let o = match self.resolve(object) {
            Resolved::Wildcard => None,
            Resolved::Id(id) => Some(id),
            Resolved::Unknown => return Vec::new(),
        };
        self.match_ids(s, p, o, None)
            .into_iter()
            .filter(|&(ts, tp, to, _)| seen.insert((ts, tp, to)))
            .filter_map(|(ts, tp, to, _)| {
                Some(Triple::new(
                    self.dictionary.term_of(ts)?.clone(),
                    self.dictionary.term_of(tp)?.clone(),
                    self.dictionary.term_of(to)?.clone(),
                ))
            })
            .collect()
    }

    /// Return every stored quad as a term-valued [`Quad`].
    pub fn all_quads(&self) -> Vec<Quad> {
        self.match_pattern(None, None, None, None)
    }

    // ------------------------------------------------------------------
    // Recovery
    // ------------------------------------------------------------------

    /// Rebuild the dictionary and permutation index by scanning the RDF data
    /// pages discovered in `allocated`.
    ///
    /// Two passes are required because a triple record references term ids
    /// that must already be in the dictionary: pass 1 replays [`TermRecord`]s
    /// (in any order — ids are explicit), pass 2 replays [`TripleRecord`]s.
    ///
    /// Pages that contain neither term nor triple records are skipped, so
    /// this is safe to call over the full allocated-page set (it ignores
    /// node/edge/property pages, which carry no RDF magic byte).
    pub fn rebuild(&mut self, pm: &PageManager, allocated: &[PageId], fs: &dyn FileSystem) {
        // Pass 1: term records → dictionary.  Collect pages that hold RDF
        // records so pass 2 does not rescan the whole database.
        let mut rdf_pages: Vec<PageId> = Vec::new();
        for &page_id in allocated {
            if page_id < 3 {
                continue; // metadata pages
            }
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            if pm.read_page(fs, page_id, &mut buf).is_err() {
                continue;
            }
            let page = SlottedPage::new(buf);
            let count = page.header().slot_count;
            let mut is_rdf_page = false;
            for slot_idx in 0..count {
                let Some(bytes) = page.read(slot_idx) else {
                    continue;
                };
                if bytes.first() == Some(&super::RDF_TERM_RECORD_MAGIC) {
                    if let Some(rec) = TermRecord::decode(bytes) {
                        self.dictionary.reinsert(rec.id, rec.term);
                        is_rdf_page = true;
                    }
                } else if bytes.first() == Some(&super::RDF_TRIPLE_RECORD_MAGIC) {
                    is_rdf_page = true;
                }
            }
            if is_rdf_page {
                rdf_pages.push(page_id);
            }
        }

        // Pass 2: triple records → permutation index + in-memory mirror.
        for &page_id in &rdf_pages {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            if pm.read_page(fs, page_id, &mut buf).is_err() {
                continue;
            }
            let page = SlottedPage::new(buf);
            let count = page.header().slot_count;
            for slot_idx in 0..count {
                let Some(bytes) = page.read(slot_idx) else {
                    continue;
                };
                if bytes.first() != Some(&super::RDF_TRIPLE_RECORD_MAGIC) {
                    continue;
                }
                let Some(rec) = TripleRecord::decode(bytes) else {
                    continue;
                };
                let slot = SlotRef::new(page_id as u32, slot_idx as u8);
                let key = (rec.subject, rec.predicate, rec.object, rec.graph);
                if self.triples.insert(key, slot).is_none() {
                    let _ =
                        self.index_insert(rec.subject, rec.predicate, rec.object, rec.graph, slot);
                }
            }
        }

        self.rdf_pages = rdf_pages;
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Intern a term, persisting a [`TermRecord`] only when it is new.
    fn intern(
        &mut self,
        pm: &mut PageManager,
        wal: &mut WalWriter,
        term: &Term,
        fs: &dyn FileSystem,
    ) -> Result<u64, RdfError> {
        let (id, is_new) = self.dictionary.intern(term.clone());
        if is_new {
            let rec = TermRecord {
                id,
                term: term.clone(),
            };
            let bytes = rec.encode();
            let _slot = self.persist_record(pm, wal, &bytes, fs)?;
        }
        Ok(id)
    }

    /// Resolve a pattern position to either a wildcard, a known id, or an
    /// unknown-term marker (which makes the whole pattern unmatchable).
    fn resolve(&self, term: Option<&Term>) -> Resolved {
        match term {
            None => Resolved::Wildcard,
            Some(t) => match self.dictionary.id_of(t) {
                Some(id) => Resolved::Id(id),
                None => Resolved::Unknown,
            },
        }
    }

    /// Materialise a `(s,p,o,g)` id tuple back into a term-valued [`Quad`].
    fn materialise(&self, s: u64, p: u64, o: u64, g: u64) -> Option<Quad> {
        let subject = self.dictionary.term_of(s)?.clone();
        let predicate = self.dictionary.term_of(p)?.clone();
        let object = self.dictionary.term_of(o)?.clone();
        let triple = Triple::new(subject, predicate, object);
        let graph = if g == 0 {
            None
        } else {
            Some(self.dictionary.term_of(g)?.clone())
        };
        Some(Quad { triple, graph })
    }

    /// Match against the in-memory triple mirror by id, honouring wildcards.
    ///
    /// The mirror is authoritative for membership; using it (rather than the
    /// permutation index) avoids any dependency on index de-keying for the
    /// graph dimension and guarantees exact `(s,p,o,g)` results.
    fn match_ids(&self, s: Pos, p: Pos, o: Pos, g: Pos) -> Vec<(u64, u64, u64, u64)> {
        self.triples
            .keys()
            .copied()
            .filter(|&(ts, tp, to, tg)| {
                s.is_none_or(|v| v == ts)
                    && p.is_none_or(|v| v == tp)
                    && o.is_none_or(|v| v == to)
                    && g.is_none_or(|v| v == tg)
            })
            .collect()
    }

    /// Insert a `(s,p,o,g)` tuple into the permutation index.
    fn index_insert(
        &self,
        s_id: u64,
        p_id: u64,
        o_id: u64,
        g_id: u64,
        slot: SlotRef,
    ) -> Result<(), RdfError> {
        let triple = RdfTriple {
            subject: s_id as u128,
            predicate: p_id,
            object: o_id as u128,
        };
        let graph = if g_id == 0 { None } else { Some(g_id) };
        self.index
            .insert(&triple, slot, graph)
            .map_err(|_| RdfError::Index)
    }

    /// Re-insert every surviving `(s,p,o,*)` tuple into the permutation index
    /// after a delete, since the permutation index is keyed on `(s,p,o)` only
    /// and a delete of one graph's `(s,p,o)` must not drop the others.
    fn reindex_spo(&self, s_id: u64, p_id: u64, o_id: u64) {
        for (&(ts, tp, to, tg), &slot) in &self.triples {
            if ts == s_id && tp == p_id && to == o_id {
                let _ = self.index_insert(ts, tp, to, tg, slot);
            }
        }
    }

    /// Write a fixed-size RDF primary record to a slotted data page, flushing
    /// the page and WAL-logging the insert.  Returns the record's [`SlotRef`].
    fn persist_record(
        &mut self,
        pm: &mut PageManager,
        wal: &mut WalWriter,
        record: &[u8],
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, RdfError> {
        // Try existing RDF pages, most recent first.
        for &page_id in self.rdf_pages.iter().rev() {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            pm.read_page(fs, page_id, &mut buf)
                .map_err(|_| RdfError::Io)?;
            let mut page = SlottedPage::new(buf);
            if let Some(slot) = page.insert(record) {
                page.update_checksum();
                pm.write_page(fs, page_id, &mut page.buf)
                    .map_err(|_| RdfError::Io)?;
                let slot_ref = Self::slot_ref(page_id, slot)?;
                Self::log(pm, wal, fs, RecordType::RdfTripleInsert, record.to_vec())?;
                return Ok(slot_ref);
            }
        }

        // Allocate a fresh RDF page.
        let page_id = pm.allocate_page();
        let mut page = SlottedPage::init(page_id, PageType::SlottedData);
        let slot = page.insert(record).ok_or(RdfError::PageFull)?;
        page.update_checksum();
        pm.write_page(fs, page_id, &mut page.buf)
            .map_err(|_| RdfError::Io)?;
        self.rdf_pages.push(page_id);
        let slot_ref = Self::slot_ref(page_id, slot)?;
        Self::log(pm, wal, fs, RecordType::RdfTripleInsert, record.to_vec())?;
        Ok(slot_ref)
    }

    /// Prepare a quad for an atomic no-steal commit: intern its terms and build
    /// the (new) term records plus the triple record into the `pending`
    /// page-image map WITHOUT writing to disk.  Returns `None` if the triple
    /// already exists (idempotent).  The caller logs one physical `PageInsert`
    /// per pending page, commits, writes the pages, and then calls
    /// [`Self::commit_prepared`] to update the in-memory index (findings C1).
    pub fn prepare_quad(
        &mut self,
        pm: &mut PageManager,
        pending: &mut std::collections::HashMap<PageId, AlignedBuffer>,
        quad: &Quad,
        fs: &dyn FileSystem,
    ) -> Result<Option<PreparedQuad>, RdfError> {
        let s_id = self.intern_pending(pm, pending, &quad.triple.subject, fs)?;
        let p_id = self.intern_pending(pm, pending, &quad.triple.predicate, fs)?;
        let o_id = self.intern_pending(pm, pending, &quad.triple.object, fs)?;
        let g_id = match &quad.graph {
            Some(g) => self.intern_pending(pm, pending, g, fs)?,
            None => 0,
        };

        let key = (s_id, p_id, o_id, g_id);
        if self.triples.contains_key(&key) {
            return Ok(None);
        }

        let record = TripleRecord::new(s_id, p_id, o_id, g_id);
        let triple_slot = self.persist_into_pending(pm, pending, &record.encode(), fs)?;
        Ok(Some(PreparedQuad { key, triple_slot }))
    }

    /// Update the in-memory permutation index and triple mirror for a quad whose
    /// pages have been durably committed.  Call only after the commit succeeds.
    pub fn commit_prepared(&mut self, prepared: PreparedQuad) -> Result<(), RdfError> {
        let (s_id, p_id, o_id, g_id) = prepared.key;
        self.index_insert(s_id, p_id, o_id, g_id, prepared.triple_slot)?;
        self.triples.insert(prepared.key, prepared.triple_slot);
        Ok(())
    }

    /// Intern a term, building a [`TermRecord`] into `pending` when it is new
    /// (no disk write).
    fn intern_pending(
        &mut self,
        pm: &mut PageManager,
        pending: &mut std::collections::HashMap<PageId, AlignedBuffer>,
        term: &Term,
        fs: &dyn FileSystem,
    ) -> Result<u64, RdfError> {
        let (id, is_new) = self.dictionary.intern(term.clone());
        if is_new {
            let rec = TermRecord {
                id,
                term: term.clone(),
            };
            let bytes = rec.encode();
            let _slot = self.persist_into_pending(pm, pending, &bytes, fs)?;
        }
        Ok(id)
    }

    /// No-steal counterpart of [`Self::persist_record`]: place `record` into the
    /// in-flight `pending` image for an RDF page (preferring an already-touched
    /// page) without writing to disk or logging.  Returns the record's slot.
    fn persist_into_pending(
        &mut self,
        pm: &mut PageManager,
        pending: &mut std::collections::HashMap<PageId, AlignedBuffer>,
        record: &[u8],
        fs: &dyn FileSystem,
    ) -> Result<SlotRef, RdfError> {
        for &page_id in self.rdf_pages.iter().rev() {
            let image = match pending.get(&page_id) {
                Some(buf) => buf.clone(),
                None => {
                    let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
                    pm.read_page(fs, page_id, &mut buf).map_err(|_| RdfError::Io)?;
                    buf
                }
            };
            let mut page = SlottedPage::new(image);
            if let Some(slot) = page.insert(record) {
                page.update_checksum();
                let slot_ref = Self::slot_ref(page_id, slot)?;
                pending.insert(page_id, page.buf);
                return Ok(slot_ref);
            }
        }

        let page_id = pm.allocate_page();
        let mut page = SlottedPage::init(page_id, PageType::SlottedData);
        let slot = page.insert(record).ok_or(RdfError::PageFull)?;
        page.update_checksum();
        self.rdf_pages.push(page_id);
        let slot_ref = Self::slot_ref(page_id, slot)?;
        pending.insert(page_id, page.buf);
        Ok(slot_ref)
    }

    /// Remove a record from its slotted page (best-effort tombstone).
    fn tombstone_record(
        &self,
        pm: &mut PageManager,
        slot: SlotRef,
        fs: &dyn FileSystem,
    ) -> Result<(), RdfError> {
        let page_id = slot.page_id() as u64;
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        pm.read_page(fs, page_id, &mut buf)
            .map_err(|_| RdfError::Io)?;
        let mut page = SlottedPage::new(buf);
        page.delete(slot.slot_index() as u16);
        page.update_checksum();
        pm.write_page(fs, page_id, &mut page.buf)
            .map_err(|_| RdfError::Io)?;
        Ok(())
    }

    /// Pack a `(page_id, slot)` into a [`SlotRef`], guarding the 24-bit page
    /// range.
    fn slot_ref(page_id: PageId, slot: u16) -> Result<SlotRef, RdfError> {
        if page_id > SlotRef::MAX_PAGE_ID as u64 || slot > SlotRef::MAX_SLOT_INDEX as u16 {
            return Err(RdfError::PageFull);
        }
        Ok(SlotRef::new(page_id as u32, slot as u8))
    }

    /// Append a WAL record for an RDF mutation and advance the superblock LSN.
    fn log(
        pm: &mut PageManager,
        wal: &mut WalWriter,
        fs: &dyn FileSystem,
        record_type: RecordType,
        payload: Vec<u8>,
    ) -> Result<u64, RdfError> {
        let prev_lsn = pm.superblock.current_wal_lsn;
        let rec = WalRecord::new(record_type, 0, 0, prev_lsn, payload);
        let lsn = wal.append(fs, rec).map_err(|_| RdfError::Io)?;
        pm.superblock.current_wal_lsn = lsn;
        Ok(lsn)
    }
}

/// Outcome of resolving a pattern position against the dictionary.
enum Resolved {
    /// A wildcard (`None` in the pattern).
    Wildcard,
    /// A bound term with a known id.
    Id(u64),
    /// A bound term not present in the dictionary — pattern cannot match.
    Unknown,
}
