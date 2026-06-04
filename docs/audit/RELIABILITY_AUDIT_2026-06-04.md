# RGraph Reliability Audit — 2026-06-04

**Scope:** a complete, deep, component-by-component reliability audit of the
RGraph crate **and** of the way its components are connected, run against the
current `main` tree at commit `00b5f5c` (post Sprints A–G). Focus: where the
crate is fragile with respect to **reliability** — Durability, Atomicity,
Isolation, Consistency, concurrency soundness, crash recovery, panic-freedom on
the live path, and resource bounds. This document is the durable registry of
findings; each finding maps to a focused remediation task in `rmp` (roadmap
`rgraph`).

**Method (empirical).** Nine parallel deep sub-audits (one per architectural
cluster plus one dedicated to cross-component seams and the test suite), each
reading the actual current source and citing `file:line` evidence; plus targeted
manual verification of the highest-impact single-source findings. No conclusion
is drawn from documentation or names alone. The prior audit
(`PRODUCTION_READINESS_AUDIT_2026-06-03.md`, remediated by Sprints A–G) was used
only to avoid re-reporting resolved issues; every finding below was re-confirmed
against the present code.

---

## 1. Executive verdict

RGraph has, since the prior audit, acquired the *primitives* required for ACID:
a CRC-checked WAL record codec with torn-record detection and proptests, an
ARIES skeleton (analysis/redo/undo), a double-write buffer, group commit, a
fuzzy checkpointer, page checksums verified on the real read paths, a
crash-safe catalog sidecar, dual-superblock recovery, an MVCC/SSI/lock-table
transaction subsystem, and a NUMA-aware buffer pool. The unit-test suite is
green.

**However, the crate is still not reliable against its own two non-negotiables
(100% ACID, 100% openCypher), because the durability/atomicity/isolation
machinery is not correctly composed on the live execution path, and the test
suite does not gate the seams.** The dominant, cross-cutting facts — each
confirmed independently by two or more sub-audits — are:

1. **Write-ahead logging is inverted and unenforced on the live path.** Graph
   mutators write (and, in the pool-less production path, `fsync`) the data page
   *before* appending the WAL record; the `Flusher`/group-commit/checkpointer
   that would enforce WAL-before-data are never instantiated outside tests; and
   `PageManager::write_page` stamps dirty frames with `rec_lsn = u64::MAX`, the
   sentinel that bypasses the ordering gate even if the flusher ran.
2. **ARIES UNDO is unsound for the engine's own records.** UNDO treats the
   first 8 payload bytes of a logical `NodeInsert`/`EdgeInsert` as a *page id*
   and zero-fills the page at `entity_id * PAGE_SIZE` — corrupting the
   superblock/bitmap/data. It is latent only because no `Begin` record is ever
   written, which simultaneously means loser transactions are *invisible* to
   UNDO and never rolled back. Both defects are real; fixing one exposes the
   other.
3. **MVCC/locking/SSI is dead on the production path.** Reads never consult a
   snapshot (storage is single-version with an in-place `DELETED` flag); the
   server serializes every write behind one global `RwLock<Graph>`; client
   `BEGIN/COMMIT` is disconnected from query execution; SSI antidependencies,
   range locks and write-set validation have zero engine callers
   (`validate_write_set` does not even exist). The requested isolation level has
   no effect on what statements read or write.
4. **Storage can destroy data on ordinary operations.** The server entry point
   unconditionally `init()`s, **clobbering an existing database on every
   restart**; the bitmap chain places slot *N*'s bitmap on top of live data
   page `2+N`, **silently corrupting any DB that grows past ~508 MiB**; and the
   "advisory lock" is a marker-file existence check with no `flock`, so
   concurrent opens race the superblock/bitmap/WAL into incoherence.
5. **The Cypher engine returns silently-wrong results and can be crashed by a
   client.** `RETURN/WITH DISTINCT` is dropped, `UNWIND` is mis-planned to an
   empty scan, ungrouped aggregation over empty input yields 0 rows instead of
   1; a string property > 256 bytes panics the engine; unbounded parser/
   evaluator recursion is a stack-overflow process abort; unbounded `[*]`
   expansion is an OOM vector; Cypher writes are not transaction-wrapped, so a
   mid-statement error leaves partial durable writes.
6. **The reliability test suite is largely self-certifying.** Durability tests
   do clean `sync`+`drop` rather than crashes; the one "uncommitted UNDO" test
   hand-builds a record shape the engine never emits (masking finding 2); the
   "snapshot isolation stress" tests exercise only the in-memory visibility
   predicate and never write a node; the loom suite is disabled by default; the
   ACID battery builds its own `TransactionManager` unconnected to the engine.

### Per-cluster reliability posture

| Cluster | Posture | Headline reliability fragilities |
|---|---|---|
| **WAL / Recovery** (`wal/`) | Not production-durable | WAL-before-data dead; UNDO corrupts entity-id pages; no `Begin` ⇒ losers un-undone; no directory `fsync`; recovery output not fsynced/checkpointed; gap-stop segment scan ignores `wal-archive/`. |
| **Buffer pool / I/O** (`buffer/`,`io/`) | Not production-safe | Eviction & runtime `FlushPage` write dirty pages with no durable-LSN gate; prefetch cache poisoning; unfenced Dekker writer/flusher handshake (UB on aarch64); fsync-EIO false durability; io_uring CQE matched by position. |
| **Storage / DB handle / catalog** (`storage/`,`db/`,`catalog/`) | Data-loss class | Bitmap chain aliases data pages (~508 MiB cap → corruption); no real inter-process lock; superblock double-write unstaggered; graph mode not persisted; `free_page` not idempotent; no bit-rot repair; `expect` panics on hot path. |
| **Transactions / MVCC** (`txn/`,`acid/`) | Isolation not enforced | Snapshot visibility/SSI/lock-table/write-set all dead on the real path; TxId resets to 1 each restart and truncates to u32 on disk; aborted≡committed in global state; ACID battery non-falsifiable. |
| **Indexing** (`index/`) | Sound for fixed keys; durability gap | Property index lost across restart (not rebuilt); var-length-key landmines (borrow-promoted separators panic, `has_room_for` ignores cap); `commit_with_indexes` durability claim false. |
| **Cypher engine** (`cypher/`,`tck/`) | Not compliant; crashable | DISTINCT dropped; UNWIND empty; empty-aggregate 0 rows; writes not atomic; recursion SO DoS; unbounded `[*]`; negative LIMIT wraps; list 3VL wrong; TCK harness has no corpus. |
| **Graph model / RDF** (`graph/`,`rdf/`) | Not crash-atomic | Multi-page mutations not atomic (half-linked graph on crash/error); property > 256 B panics; List/Map/temporal stored as NULL; plain DELETE orphans edges; SET leaves stale id-0 index entries; global single-writer lock. |
| **Server / runtime / CLI** (`server/`,`runtime/`,`main.rs`,`cli/`) | Two showstoppers | `init()`-on-restart clobber; txn API disconnected from execution; slow-loris on serial untimed accept loop; unbounded result buffering; graceful-drain dead code. |

**Confidence.** The five highest-impact single-source findings (bitmap aliasing,
`init()`-clobber, DISTINCT-drop, UNWIND-misplan, property-256 panic) were
verified by hand against the current code. The systemic findings (WAL inversion,
UNDO corruption, MVCC-dead, txn-disconnected) were each reported independently by
two or more sub-audits.

---

## 2. Finding register

Severity key: **C** = Critical (data loss / ACID violation / UB / crash on a
normal path), **H** = High, **M** = Medium, **L** = Low. Each finding maps to an
`rmp` task in the indicated remediation sprint (Section 3).

### Critical

- **C1 — WAL-before-data is inverted and unenforced on the live path.** Mutators
  `write_page` (pool-less: `write_at`+`sync_data`) before `log(...)`; the
  `Flusher`/group-commit/checkpointer are never instantiated in production;
  `write_page` stamps `rec_lsn = u64::MAX`. `graph/engine.rs:1592-1641`,
  `storage/manager.rs:324-348`, `buffer/flusher.rs:55`, `buffer/pool.rs:705`.
- **C2 — ARIES UNDO zero-fills entity-id-numbered pages.** UNDO reads the first
  8 payload bytes of `NodeInsert`/`EdgeInsert` as a page id and writes a zeroed
  page at `entity_id*PAGE_SIZE`. `wal/aries.rs:582-629,745-786`;
  payload at `graph/engine.rs:1621-1630`.
- **C3 — No `Begin` WAL record ⇒ loser transactions invisible to UNDO.** A txn
  that logged an insert but crashed before commit gets no ATT entry, so its
  durable-but-uncommitted data is never rolled back (and is resurrected by
  `rebuild_indexes`). `txn/manager.rs:282`, `wal/aries.rs:418-434`.
- **C4 — MVCC visibility is dead on the read path.** `get_node`/scans read raw
  single-version records; `Snapshot::is_visible`, `TupleHeader` xmin/xmax and
  infomask hints never participate in any read. Non-repeatable reads.
  `graph/engine.rs:1671-1681`, `txn/snapshot.rs:89`.
- **C5 — SSI / first-committer-wins are dead.** `record_phantom_write`,
  `record_rw_antidependency`, `acquire_range_lock`, `record_write` have zero
  engine callers; `validate_write_set` does not exist; `is_doomed` is always
  false in production. Write skew and phantoms undetectable. `txn/manager.rs:759,
  863,676`, `txn/phantom.rs:61`.
- **C6 — Server startup always `init()`s ⇒ clobbers an existing DB on every
  restart.** The `data_path.exists()` check only logs; `GraphEngineAdapter::init`
  rewrites a fresh empty superblock. Committed data does not survive a server
  restart. `main.rs:338-344`, `server/storage.rs:450-456`.
- **C7 — Bitmap chain pages alias live data pages (~508 MiB corruption cap).**
  Slot *N*'s bitmap is placed at physical page `FIRST_BITMAP_PAGE_ID + N` =
  `2+N`, but page `3+` is live data; the moment the DB needs a second bitmap it
  overwrites/​misreads data page 3. `storage/manager.rs:224-233`.
- **C8 — No real inter-process/handle lock.** The lock is a marker-file
  existence check; no `flock`/`fcntl`. Concurrent opens hand out the same page
  ids and overwrite each other's superblock/bitmap/WAL/catalog.
  `db/database.rs:285-292`.
- **C9 — Cypher writes are not transaction-wrapped ⇒ partial durable writes.**
  `execute_cypher`/`execute_full_pipeline` run write operators directly against
  the engine with no begin/commit/rollback; a mid-statement error leaves earlier
  `CREATE`s durable. `server/storage.rs:614-648`, `main.rs:615-636`,
  `cypher/physical.rs` (Create/Set/Merge/Delete ops).
- **C10 — Server transaction API is disconnected from query execution.**
  `QueryRequest` has no `tx_id`; `execute_cypher` ignores `open_txns`. `Begin →
  Execute → Rollback` does not roll back; multi-statement atomicity/isolation are
  unavailable over the wire. `proto/rgraph.proto:57-62`, `server/storage.rs:614-676`,
  `server/grpc.rs:185`.
- **C11 — `RETURN/WITH DISTINCT` silently ignored.** The planner never reads
  `ret.distinct`; no `Distinct` operator exists. Duplicate rows returned.
  `cypher/planner.rs:399-541`, `cypher/plan.rs`, `cypher/physical.rs`.
- **C12 — `UNWIND` mis-planned to a bogus `__UNWIND_*` label scan ⇒ 0 rows.**
  A working `UnwindOp` exists but the planner never emits an `Unwind` operator.
  `cypher/planner.rs:192-207`.
- **C13 — Ungrouped aggregation over empty input yields 0 rows instead of 1.**
  `AggregateOp` iterates groups; empty input ⇒ empty output, so
  `MATCH (n:Missing) RETURN count(*)` returns nothing. `cypher/physical.rs:1808-1844`.
- **C14 — Property value > 256 bytes panics the engine on the user write path.**
  `PropertyRecord::inline` `assert!`s `len <= 256`; all write paths use it; the
  overflow-page machinery is never used. `CREATE (n {s:'<300 chars>'})` aborts
  the worker. `graph/record.rs:400-405`, `graph/graph.rs:164,210,663,690,764`.

### High

- **H1 — Property index lost across restart.** `rebuild_indexes` repopulates
  node/edge/label/type but never the property index; `scan_property_index`
  returns empty after reopen. `graph/engine.rs:462-537`.
- **H2 — Property type fidelity loss.** List/Map/Date/Duration/Point silently
  stored as NULL (`to_value_type_payload` → `None` → `unwrap_or(NULL)`).
  `graph/property.rs:130-148`, `graph/graph.rs:161-163,…`.
- **H3 — Plain `DELETE n` on a connected node orphans edges.** Non-detach branch
  tombstones the node without checking/unlinking incident edges; traversals from
  the surviving endpoint return the deleted node. `cypher/physical.rs:1354-1372`,
  `graph/engine.rs:1683-1759`.
- **H4 — Unbounded parser/evaluator recursion ⇒ stack-overflow process abort.**
  No depth bound in `parse_expression`/`evaluate`/semantic check; deep nesting
  SIGSEGVs the whole process (uncatchable). Remote DoS. `cypher/parser.rs:1004-1093`,
  `cypher/interpreter.rs:103`.
- **H5 — Unbounded variable-length `[*]` expansion ⇒ OOM/hang.** `max_hops`
  defaults to `u32::MAX`, per-path clones of `path`+`visited`, no row/length
  budget. `cypher/physical.rs:937-1007`.
- **H6 — Slow-loris on the serial accept loop.** `peek` and the TLS handshake
  `await` untimed inside the single sequential `accept()`; one silent client
  freezes all new connections. `server/acceptor.rs:45-47,311-318,355-382`.
- **H7 — Checkpoints never happen in production ⇒ unbounded WAL + full replay
  every restart.** The no-pool branch resets the counter without updating
  `last_checkpoint_lsn` or truncating segments. `graph/engine.rs:899-918`.
- **H8 — WAL directory never `fsync`ed after segment create / symlink rename.**
  Committed records in a freshly rotated segment can vanish on power loss; no
  `sync_dir` exists in the `FileSystem` trait. `wal/writer.rs:235-236,440-442,
  474-491`, `io/mod.rs:20-40`.
- **H9 — `prefetch_range` cache poisoning.** Skips resident interior pages but
  reads contiguously from one base offset and publishes neighbours under the
  wrong key; `verify_checksum_bytes` never checks the header page id. Wrong/
  corrupt data. `buffer/pool.rs:507-563,574`.
- **H10 — Writer/flusher byte-exclusion is an unfenced Dekker handshake.** Pin
  store is Relaxed, flusher `io_inflight` store is not Release/SeqCst; a
  StoreLoad reorder lets both proceed → overlapping `&mut`/`&` to the same frame
  buffer (UB) on weak-memory aarch64. `buffer/pool.rs:58-64,741-755`,
  `buffer/frame.rs:238-248`.
- **H11 — TxId resets to 1 every restart and truncates to u32 on disk.** The
  manager is reconstructed from bootstrap on `init`/`open`; `persist`/`recover`/
  `RECOVERY_BUMP` are dead; `TupleHeader.xmin/xmax` are `u32` with silent `as
  u32` truncation and no epoch. `txn/mvcc.rs:67-92`, `graph/engine.rs:328,419`,
  `txn/state.rs:224`.
- **H12 — `next_free_page_id` not synced per write on the server path ⇒ page
  double-allocation after crash.** `execute_cypher` never calls `sync()`; on
  reopen the allocator derives from the stale superblock and is not reconciled to
  the real high-water mark, so the next write can re-hand-out an in-use page.
  `storage/manager.rs:206-235`, `server/storage.rs:626-648`.
- **H13 — Aborted txns become visible once `global_xmin` advances.** `commit_tx`
  and `abort_tx` both just `remove_active`; visibility trusts `txid < xmin` as
  "committed". Latent (gated by C4) but a correctness trap for whoever wires
  MVCC. `txn/state.rs:136-165`, `txn/snapshot.rs:144-146`.
- **H14 — SET leaves stale secondary-index entries; `property_key_id` always 0.**
  `write_property_chain` adds new index entries without deleting old ones and
  keys every property under id 0, so value scans return stale + cross-property
  false positives, and old property records leak. `graph/graph.rs:716-771,171-178`.
- **H15 — ARIES REDO/UNDO writes never `fsync`ed; no post-recovery checkpoint.**
  Recovery is self-healing across clean restarts but does not establish
  durability, and `last_checkpoint_lsn` is never advanced, so each crash re-does
  unbounded work. `wal/aries.rs:726,760-780`, `graph/engine.rs:396-403`.
- **H16 — ACID isolation battery is non-falsifiable.** `run_isolation` ignores
  its `graph`/`fs` args and builds a fresh `TransactionManager`; it stays green
  even if engine-level locking is removed. `acid/mod.rs:174-383`.

### Medium

- **M1 — Superblock double-write has no intervening fsync** (both copies same
  generation; a torn/partial write can defeat mirror recovery).
  `storage/manager.rs:363-374`.
- **M2 — Graph mode (LPG/RDF) is not persisted/validated on open** — an RDF DB
  can be reopened as LPG. `db/database.rs:277-308`, `storage/meta.rs:19-46`.
- **M3 — Catalog persist lacks a directory `fsync` after `rename` and reuses a
  fixed temp name** (rename not crash-durable; concurrent-persist temp race).
  `catalog/mod.rs:277-288`.
- **M4 — `free_page` is not idempotent** (double-free pushes the id twice →
  double-allocation; no metadata/bounds guard). `storage/manager.rs:242-254`.
- **M5 — No runtime bit-rot repair; `rebuild_indexes` silently skips corrupt
  pages**, dropping records from indexes with no surfaced error.
  `storage/manager.rs:279-285`, `graph/engine.rs:473-475`.
- **M6 — `expect("data file not open")` panics on every storage hot path**
  instead of returning `io::Error`. `storage/manager.rs:276,303,332,368,378,400`.
- **M7 — Direct-write doublewrite `clear()` result discarded** (`let _ =`), so a
  failed clear can let recovery reinstate a stale page. `storage/manager.rs:341-353`.
- **M8 — Delete-borrow can grow a separator and panic via `build_branch`
  `.expect`** with variable-length keys. `index/btree.rs:962-970,1068-1086`.
- **M9 — `has_room_for` ignores `MAX_INLINE_RECORD_LEN`** ⇒ a large index record
  passes the space check then panics in `insert_raw_at`. `index/page.rs:377-385`,
  `index/btree.rs:513-517`.
- **M10 — fsync-EIO lost-write: per-flush fresh fd makes a retried fsync report
  false durability**; the failure is not surfaced to the txn boundary.
  `buffer/pool.rs:775-791`, `buffer/flusher.rs:261-287`.
- **M11 — io_uring completions matched by position, not `user_data`**; an error
  after submit (or an extra CQE) reaps a stale completion as the current result.
  `io/uring.rs:191-213,243-265,315-326`.
- **M12 — Negative `SKIP`/`LIMIT` wraps to ~unlimited; float `LIMIT` accepted.**
  `n as u64` on a negative i64. `cypher/physical.rs:571,615`.
- **M13 — List equality with NULL elements returns `false` instead of `null`**
  (3VL violation). `cypher/value.rs:295-305`.
- **M14 — `size()`/`length()` merged and mis-applied to entities** (return a
  number where the spec requires a type error). `cypher/interpreter.rs:831-841`.
- **M15 — `rand()`/`timestamp()`/temporal functions are constant stubs** that
  return fixed/passthrough values instead of erroring. `cypher/interpreter.rs:819,
  993-998`.
- **M16 — Wound-wait victim keeps its non-contested locks** until it next touches
  the lock table, blocking unrelated txns. `txn/manager.rs:574-591`.
- **M17 — `acquire_lock` blocks on `recv()` with no timeout** ⇒ a lost wakeup /
  lingering-lock chain can hang a blocking-pool worker permanently.
  `txn/manager.rs:610-636`.
- **M18 — Poison-on-panic policy across the txn subsystem** turns one panic into
  a cascading process-killing failure for all subsequent transactions.
  `txn/state.rs`, `txn/lock_table.rs`, `txn/wound_wait.rs`, `txn/phantom.rs`,
  `txn/manager.rs`.
- **M19 — `ExecutionContext` launders `&mut` from `&` via `UnsafeCell<*mut>`
  while a live `&engine` aliases it** (UB even single-threaded).
  `cypher/physical.rs:59-129`.
- **M20 — TCK harness has no real openCypher corpus and under-validates**
  (`Empty` passes on any non-error; NULL can never match). Compliance is
  unmeasured. `tck/mod.rs`.
- **M21 — No `Drop` flush; `let _ = rebuild_indexes` swallows recovery failure**
  ⇒ silent incomplete indexes / unflushed superblock on handle drop.
  `db/database.rs`, `graph/engine.rs:439`.
- **M22 — Torn-tail policy diverges between recovery readers; a torn tail is left
  on disk** and future appends/recoveries stop at it, silently dropping later
  records. `wal/recovery.rs:62-74`, `wal/aries.rs:257-266`.
- **M23 — `load_all_segments` stops at the first gap and ignores `wal-archive/`**
  ⇒ a checkpoint LSN inside an archived segment truncates recovery.
  `wal/writer.rs:268-285,521-540`, `wal/aries.rs:224-232`.
- **M24 — Checkpoint encodes `rec_lsn = u64::MAX` pages into the DPT** (and never
  flushes them), so checkpoints don't bound recovery. `wal/checkpoint.rs:44-85`.
- **M25 — `extract_before_image` backward magic-scan can false-match `BIMG`
  bytes inside a page image**, restoring a bogus before-image during UNDO.
  `wal/aries.rs:816-844`.
- **M26 — `ORDER BY` swallows eval errors (→ NULL key) and treats incomparable
  types as `Equal`**, giving arbitrary order instead of the spec's orderability.
  `cypher/physical.rs:520-526`, `cypher/value.rs:393-421`.
- **M27 — `find_free_frame` is an unbounded spin/yield loop** that livelocks when
  all frames are pinned or only WAL-blocked dirty victims remain.
  `buffer/pool.rs:618-627`.
- **M28 — TOCTOU between the `AlreadyExists` check and the exclusive lock in
  `put_node`** (the create is not actually guarded by the lock).
  `graph/engine.rs:1575-1587`.
- **M29 — No explicit gRPC message-size limit and unbounded result buffering**
  ⇒ a large `MATCH … RETURN n` materialises fully in RAM (OOM).
  `server/grpc.rs:140-150,451-535`.
- **M30 — Graceful-shutdown drain machinery is dead code** (`in_flight_count`
  has no producer); shutdown works only because tonic drains internally.
  `server/runtime.rs:139-184`, `main.rs:369`.
- **M31 — CLI `Insert` one-shot path panics on oversized data / any I/O error**
  (`.expect` instead of mapped exit code). `main.rs:217-238`.
- **M32 — `SlotRef::new` truncates page ids > 2^24 silently in release on the
  rebuild path** (`page_id as u32`, `debug_assert!` only). `graph/record.rs:48-54`,
  `graph/engine.rs:495,515`, `rdf/store.rs:395`.
- **M33 — RDF `delete` ignores POS/OSP "not found" and inserts are non-atomic**
  ⇒ permutation indexes can diverge (phantom triples). `index/rdf_store.rs:128-156`.
- **M34 — LPG `rebuild_indexes` can mis-decode RDF records as nodes/edges by
  length** (no magic-byte guard) ⇒ phantom nodes/edges + inflated id allocator.
  `graph/engine.rs:462-537`.
- **M35 — `is_doomed` uses a non-pivot SSI rule** (needs both flags on the same
  txn) ⇒ misses dangerous structures once wired. `txn/phantom.rs:88-94`.
- **M36 — Wound callback aborts the victim in global state before any WAL Abort
  record**, and the self-abort fast-path writes none ⇒ divergent abort paths.
  `txn/manager.rs:574-591,515-526`.

### Low

- **L1 — `toInteger`/`toFloat`/`toBoolean` on an unparseable string error
  instead of returning `null`.** `cypher/interpreter.rs:611-628`.
- **L2 — CSR `row_ptr` sized by `max_node_id`** ⇒ a single high id forces a
  gigabyte allocation. `graph/csr.rs:156-197`.
- **L3 — `PropertyRecord`/`Term` length fields truncate via `as u16`.**
  `graph/record.rs:437,442`, `rdf/term.rs:185,187`.
- **L4 — Secondary-index scan descent uses `sep < key` vs the tree's `sep <=
  key`** (latent; self-correcting for forward scans only). `index/label.rs:142`,
  et al.
- **L5 — `split_leaf`/`merge_*` discard the `insert_raw` Result** (silent entry
  loss if a half ever overflows). `index/btree.rs:820-828,1268-1313`.
- **L6 — `alloc_page` page-id×PAGE_SIZE overflow + dropped `set_len` errors**
  ⇒ a missing child can route to page 0. `index/btree.rs:191-206`.
- **L7 — Range cursor holds the coarse structural latch for its lifetime**
  (liveness: blocks all writers to the tree). `index/btree.rs:708,762`,
  `index/cursor.rs:191-199`.
- **L8 — `seed_dpt_from_checkpoint` uses `.expect` on slice conversions on the
  recovery path** (defensive). `wal/aries.rs:897-906`.
- **L9 — `WalWriter::flush` derives the write offset from `handle.len()`**, which
  diverges from LSN-as-offset after a descriptor block. `wal/writer.rs:354-356`.
- **L10 — `unfix_page` underflow clamp is racy** (non-atomic clamp vs concurrent
  RMW). `buffer/pool.rs:602-609`.
- **L11 — `submit_and_wait` does not retry on `EINTR`.** `io/uring.rs:191,243,315`.
- **L12 — Checksum helpers `assert_eq!`/panic on the I/O path**, killing the
  flusher thread silently. `storage/page.rs:196,205,212`.
- **L13 — `mbind` uses `MPOL_MF_STRICT` and silently falls back** ⇒ NUMA pinning
  is a no-op with no observability. `buffer/numa.rs:160-181`.
- **L14 — `decode_superblock` conflates version/endianness mismatch with
  corruption; on-disk format is non-portable native-endian.** `storage/meta.rs:86-122`.
- **L15 — Storage page-id arithmetic lacks overflow guards / capacity checks.**
  `storage/manager.rs:216-217,277,…`.
- **L16 — Commit-failure is counted as "aborted" in metrics.** `server/grpc.rs:321-330`.
- **L17 — `IoScheduler::call_sync` panics if a worker dies** (latent; dead in
  prod, unlike the hardened `IoBridge`). `runtime/scheduler.rs:301-305`.
- **L18 — `insert_batch_logged` is unused ⇒ `commit_with_indexes`'s index-
  durability claim is false** (masked today by rebuild-on-open). `index/btree.rs:374-450`,
  `txn/manager.rs:391-415`.

---

## 3. Remediation roadmap (sprints by magnitude of gains)

Sprints continue the A–G scheme and are ordered by magnitude of reliability
gains; lower sprints are foundational and unblock the rest. Each finding becomes
one focused `rmp` task (BUG/IMPROVEMENT) carrying the objective description,
reproduction, `file:line` location, and a **regression-test gate** in its
acceptance criteria.

- **Sprint H — Durability Spine II (WAL-before-data, ARIES UNDO, checkpointing).**
  C1, C2, C3, H7, H8, H15, M22, M23, M24, M25, L8, L9, L18.
- **Sprint I — Storage & Process Integrity.** C6, C7, C8, H12, M1, M2, M3, M4,
  M5, M6, M7, L14, L15.
- **Sprint J — Transaction Isolation & ACID Integration.** C4, C5, C9, C10, H11,
  H13, H16, M16, M17, M18, M28, M35, M36.
- **Sprint K — Concurrency & Memory-Model Soundness.** H9, H10, M10, M11, M19,
  M21, M27, L10, L11, L12, L13.
- **Sprint L — Cypher Correctness, Compliance & Robustness.** C11, C12, C13, H4,
  H5, M12, M13, M14, M15, M20, M26, L1.
- **Sprint M — Graph & Index Model Correctness/Durability.** C14, H1, H2, H3,
  H14, M8, M9, M32, M33, M34, L2, L3, L4, L5, L6, L7.
- **Sprint N — Server Availability & CLI Hardening.** H6, M29, M30, M31, L16, L17.

**Cross-cutting test-suite mandate** (applies to every sprint): replace the
self-certifying gates — drive durability tests through real crash injection
(`io/fault.rs`), drive isolation tests through the engine/server API (not a
standalone `TransactionManager`), enable the loom job in CI, and gate property/
index/schema durability across an actual reopen.

---

*Generated by the 2026-06-04 reliability audit. The full per-finding evidence
(quotes, reproductions, regression-test designs) is retained in the audit run and
distilled into the corresponding `rmp` tasks.*
