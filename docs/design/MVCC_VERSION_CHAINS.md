# Design: On-disk MVCC version chains (Sprint J, tasks 220 / 221)

Status: IN PROGRESS (2026-06-05). Decision: **full on-disk version chains**
(user-approved, "full chains in one push").

## Goal

Make storage multi-version so snapshot isolation is real on the read path. Close
task 220's acceptance test:

> begin RepeatableRead, read N=v0; another txn commits N=v1; re-read within the
> first txn and assert it still returns v0.

Then wire SSI (task 221): record read-ranges / antidependencies / write-sets on
the engine path, implement `validate_write_set`, validate at commit (first-
committer-wins + dangerous-structure), so two Serializable txns that write-skew
have exactly one abort.

## Current state (from the 2026-06-05 terrain map)

- On-disk records are raw single-version `NodeRecord` (32 B) / `EdgeRecord`
  (64 B); the slot holds exactly that. `get_node`/`get_edge` decode the record
  and only check the in-place `DELETED` flag — no snapshot.
- A 16-byte `TupleHeader` (xmin u32, xmax u32, cid u16, infomask u16,
  next_version_ptr u32) exists in `txn/mvcc.rs` but is written **only to WAL
  payloads**, never to disk.
- `ExecutionContext` carries no txid/Snapshot.
- `Snapshot::is_visible(xmin, xmax, xmin_committed, xmax_committed)` exists and
  (after H13) consults the abort log via `with_aborted`.
- The TransactionManager SSI/lock primitives (`record_write`,
  `record_phantom_write`, `acquire_range_lock`, `record_rw_antidependency`) have
  zero engine callers; `validate_write_set` does not exist; `TxError::WriteConflict`
  exists.

## On-disk format

A versioned record slot stores `[VersionHeader][entity record]`:

```
VersionHeader (24 bytes, repr(C)):
  0x00  xmin          u64   creating TxId (64-bit; avoids the u32 truncation of TupleHeader, task 224)
  0x08  xmax          u64   deleting/superseding TxId (0 = live, not deleted)
  0x10  next_version  SlotRef (u32)  older version of this entity (NULL = oldest)
  0x14  flags         u16   infomask (XMIN_COMMITTED/ABORTED, XMAX_COMMITTED/ABORTED)
  0x16  _pad          u16
```

So a node version slot = 24 + 32 = **56 B**; an edge version slot = 24 + 64 =
**88 B**. `FORMAT_VERSION` bumps to 2 (pre-alpha; existing files rebuilt).

Chain direction: the secondary index (`node_index`/`edge_index`) points to the
**newest** version (the head). `next_version` links newest → … → oldest. A reader
walks from the head and returns the first version whose `is_visible(xmin, xmax,
snapshot)` is true.

## Localisation strategy (keep the blast radius small)

`Self::read_record(slot_ref)` is the single choke point for reading an entity
slot. It will **strip the 24-byte header** and return the entity bytes, so every
existing `NodeRecord::decode`/`EdgeRecord::decode` caller is unchanged. A new
`read_record_versioned(slot_ref)` returns `(VersionHeader, entity_bytes)` for the
version-aware read path. The write paths (`prepare_record`, `create_*_atomic`,
the SET/update path, delete) **prepend** the header. `rebuild_indexes`' slot
dispatch strips the header before matching entity size (56 → node, 88 → edge).

## Increment plan (each commit builds + passes)

1. **Format foundation (inert).** Add `VersionHeader` + combine/split helpers.
   `read_record` strips the header; write paths prepend it with
   `xmin = creating txid, xmax = 0, next = NULL`. `rebuild_indexes` dispatch
   updated. FORMAT_VERSION → 2. Behaviour unchanged (single version); suite green.
2. **Snapshot-aware visibility (create/delete).** Thread `Option<Snapshot>` +
   owner txid through `ExecutionContext`; `get_node`/`get_edge`/scans consult
   `is_visible(xmin, xmax)`. Delete sets `xmax = txid` on the head version. Fixes
   dirty/aborted reads and delete-visibility across snapshots.
3. **Update version chains.** SET writes a NEW head version (new first_property)
   whose `next_version` points to the old head, sets the old head's `xmax = txid`,
   and repoints the index to the new head. Reads walk the chain → closes 220's
   UPDATE gate. Recovery is mostly free (no-steal logs full page images).
4. **Vacuum (optional/follow-up).** Reclaim versions older than the global xmin.
5. **SSI (task 221).** Record write-sets/read-ranges/antidependencies on the
   engine write path; implement `validate_write_set` (first-committer-wins) and
   call it + the dangerous-structure (pivot) check in `commit` for Serializable.
   Engine/server write-skew harness asserts exactly-one-abort.

## OBSTACLE discovered during write-path design (2026-06-05): intrusive adjacency vs MVCC

`NodeRecord`/`EdgeRecord` carry **intrusive adjacency linked-lists**
(`first_outgoing_edge`, `first_incoming_edge`, and `prev/next_source_edge`,
`prev/next_target_edge`). Versioning the whole record collides with this:

- A node's two versions (differing in `first_property`) both contain adjacency
  head pointers. They must stay consistent, so either adjacency is duplicated
  across versions (and every adjacency change rewrites all versions) or it is
  factored out.
- An edge insert mutates the **endpoints' adjacency heads** and the neighbour
  edges' `prev/next` pointers. Under whole-record versioning, that insert would
  have to create new versions of the endpoint nodes and the spliced edges, and a
  snapshot traversal would have to follow only the edge versions visible to it —
  a deep, subtle change to every adjacency walk.

A property store (PostgreSQL) never hits this because relationships are separate
rows, not intrusive pointers. This makes "version the whole record" much larger
and riskier than a header prepend.

### DECISION (2026-06-05, user): **Version values only (option 1).**

Concretely: the on-disk record still carries the whole `NodeRecord`/`EdgeRecord`
behind a `VersionHeader`, but the two read modes are split:

- **Value reads** (label / properties — `get_node`, scans, property walks) walk
  the version chain and return the **snapshot-visible** version.
- **Adjacency reads/writes** (scan_outgoing/incoming, edge splice on insert)
  always use the **head (latest) version** in place — topology is effectively
  single-version, so an edge insert mutates the head's adjacency without minting
  a new version, and adjacency walks are never snapshot-filtered.
- **Value UPDATE (SET)** mints a new head version (copying the head's topology,
  carrying the new `first_property`), chains `next_version` to the old head, sets
  the old head's `xmax`, and repoints the index to the new head.

This closes 220's property UPDATE gate without re-architecting adjacency. (A
follow-up can make topology MVCC-isolated — option 2 — if write-skew on adjacency
ever needs catching.)

### Resolution options (history)

1. **Version only the value part; keep topology single-version.** Split each
   record into an immutable-topology part (adjacency pointers) that stays
   single-version + in-place, and a versioned value part (`label_id`,
   `first_property`). The version chain links value parts only; adjacency walks
   are unchanged. Closes 220's *property* UPDATE gate (the acceptance test is
   about property values v0/v1) without re-architecting adjacency. **Recommended**
   — smallest correct change that satisfies the gate.
2. **Full whole-record versioning** (adjacency included). Correct and complete,
   but every edge insert versions its endpoints, and every adjacency traversal
   becomes snapshot-filtered. Largest effort; touches the CSR and all scans.
3. **Move adjacency out of the record** (separate adjacency store / rely on the
   CSR), then version the now-topology-free record freely. Clean long-term, but a
   broad storage refactor.

## Risks / notes

- Recovery: PageInsert logs full page after-images, so version headers ride along
  for free; REDO needs no special handling.
- Indexes store `node_id → SlotRef(head)`; on update the index is repointed to the
  new head atomically within the same no-steal transaction.
- TxId durability (task 224) and the u32→u64 widening are partially advanced here
  (the on-disk header is 64-bit) but full persistence of the TxId counter remains
  task 224.
