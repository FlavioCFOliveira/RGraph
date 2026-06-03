# RGraph Production-Readiness Audit — 2026-06-03

**Scope:** complete, component-by-component audit of the RGraph crate to determine
its degree of completeness/reliability for real use, followed by the connections
between components. This document is the durable registry of findings; every
finding is resolved by a task in the remediation roadmap (see the last section),
organised into sprints by **magnitude of gains**.

**Method (empirical):** 8 parallel deep audits (one per architectural cluster),
each reading the actual source and citing `file:line` evidence; plus
`cargo check --all-targets`, `cargo clippy`, static scans for
`todo!/unimplemented!/unreachable!/panic!/unwrap`, and the Knowledge Graph
dependency structure. No conclusion is drawn from documentation or names alone.

---

## 1. Executive verdict

**RGraph is pre-alpha relative to its own non-negotiable requirements. It is NOT
ready to be used with reliability.** Neither *100% ACID* nor *100% openCypher
compliance* is met, and neither is currently *measurable* on the real execution
path.

The single dominant, cross-cutting fact — confirmed independently by all eight
audits — is **disconnection**: the crate is a large collection of individually
well-unit-tested building blocks that are **not wired together** on the
production path. The advanced machinery the design depends on exists only as
dead or test-only code:

| Capability built | State on the real path | Evidence |
|---|---|---|
| Page checksums (torn-write/bit-rot defence) | Never invoked by buffer pool / storage I/O | `buffer/pool.rs`, `buffer/flusher.rs` call `read_at`/`writev_at` with no `verify_checksum`/`update_checksum` |
| ARIES recovery (analysis/redo/**undo**/CLR) | Never invoked; startup uses redo-only of **all** records incl. uncommitted | `graph/engine.rs:228`, `db/database.rs:116` call `recover()`; `AriesRecovery` has no non-test caller |
| Double-write buffer (torn-page) | Never staged before page writes | no caller of `DoubleWriteBuffer` outside its module |
| Group commit (durable throughput) | Never used; per-txn fsync | no caller of `GroupCommitQueue::submit` |
| Fuzzy checkpoint / bounded recovery | Never invoked; `last_checkpoint_lsn` stays 0 | no caller of `Checkpoint::run` |
| MVCC / lock table / wound-wait | Zero callers outside `src/txn/`; server uses last-writer-wins map | `server/storage.rs` `InMemoryTransaction` |
| Buffer pool for indexes | Secondary indexes are RAM-only, lost on restart | `index/manager.rs:94` builds trees without a pool |
| Graph property persistence | Properties dropped on create, empty on read | `graph/graph.rs:65,84,99,…` TODOs |
| Cypher planner→physical→storage | Dead code; CLI/server use a RETURN-literal-only executor | `main.rs:367`, `server/grpc.rs:268` |
| openCypher TCK harness | Stub: never parses/executes/compares | `tck/mod.rs:191-241` TODOs; `run_scenario` never called |

Because of this, a green test suite (≈637 unit tests) is **misleading**: most
high-risk paths (multi-page persistence, concurrency, crash recovery, real query
execution, isolation anomalies) are exercised only by mocks or single-threaded
happy paths, and several "ACID" tests are self-certifying placeholders that
return `Ok` without testing anything.

### Build/quality ground truth
- `cargo check --all-targets` **FAILS**: `benches/btree_bench.rs` does not
  compile (`search()` expects `&CompositeKey`, bench passes `&Vec<u8>`). CI is red.
- `cargo clippy --lib --bins`: **115** warnings (collapsible-if, dead code,
  unused variables/imports, `clone` on `Copy`, etc.).
- Static markers in `src/`: 3 `unreachable!`, 13 `TODO/FIXME`, 43 `panic!`,
  0 `#[ignore]`.

---

## 2. Per-cluster completeness & readiness

Percentages are the auditors' empirical completeness estimates toward
production reliability (not lines written).

| Cluster | Overall | Headline blockers |
|---|---|---|
| **Storage & I/O** (`io/`,`storage/`,`buffer/`) | ~55% | Checksums never invoked on I/O path; `fdatasync` only (no `sync_all` after file growth); single hard-coded bitmap caps DB at ~508 MiB; buffer-pool `UnsafeCell` aliasing unsound under concurrency; `find_free_frame` can livelock/stack-overflow; PageManager I/O bypasses & races the pool. |
| **WAL / Recovery / ACID** (`wal/`,`acid/`) | ~30% | Full ARIES/double-write/group-commit/checkpoint are dead code; redo-only recovery redoes uncommitted work and has no UNDO before-images; page_lsn endianness bug; recovery reads only first WAL segment; ACID gate self-certifies via `Ok`-returning placeholders. |
| **Indexing** (`index/`) | ~40% | No WAL logging / no buffer pool for index pages (durability fails; secondary indexes lost on restart); single root latch dropped before split, readers unlatched (isolation fails); branch split broken in pool mode (`find_parent`); no merge/rebalance; bulk loader produces unsearchable tree; 40-byte/16-byte key truncation breaks ordering; RDF POS/OSP scans scramble fields. |
| **Transactions / MVCC** (`txn/`) | ~20% | Entire subsystem unwired (zero callers outside `txn/`); production path is last-writer-wins with no conflict detection; tuple versions never WAL-logged; no version chains/GC; SSI write-skew detection dead; wound-wait never aborts victims; immunity can re-introduce deadlock; 32-bit on-disk TxId with no freeze (wraparound corruption). |
| **Graph model / DB** (`graph/`,`db/`,`id.rs`,`config.rs`) | ~30% | Properties never persisted or read; Cypher CREATE writes nothing; logical WAL records never replayed; adjacency lists never maintained (traversal impossible); endpoint slots truncate node ids > 2^24; hard-coded txid=1; no MVCC/isolation/atomicity; `Database`/`GraphBuilder::build` produce a handle with no graph ops; RDF entirely absent; single global `Mutex` defeats concurrency. |
| **Cypher frontend** (`lexer/parser/ast/semantic/value`) | ~35% | logos lexer is dead code (parser hand-rolls a weaker scanner); missing WITH, UNWIND, OPTIONAL MATCH, FOREACH, CALL, UNION, var-length `[*m..n]`, parameters `$p`, CASE, comprehensions, reduce, `RETURN *`, multi-part patterns; no temporal/spatial/entity types; 3VL wrong; `!=` mis-mapped to `=`; DISTINCT-in-aggregate dropped; equality `1=1.0` wrong. |
| **Cypher backend / TCK** (`plan/planner/physical/executor/interpreter`,`tck/`) | ~20% | planner→physical→executor is dead code (CLI/server use naive RETURN-only executor); ExpandOp inert; scans don't bind variables or load properties; label ids can't match storage; CREATE/SET/REMOVE/DELETE/MERGE/Apply/HashJoin are no-op stubs; aggregate over empty input yields 0 rows; columns alphabetised (RETURN order lost); TCK harness is a stub never invoked — **compliance is unmeasured**. |
| **Server / runtime** (`server/`,`runtime/`,`main.rs`,…) | ~35% | gRPC answers only literal RETURN; engine never invoked over the wire; ConnectionAcceptor (TLS, conn limit, mux, slow-loris) and RequestDispatcher are constructed then bypassed; `--tls` silently serves cleartext; graceful-shutdown drain is a no-op; `call_sync` hits a reachable `unreachable!()`; request arithmetic panics on `1/0`/overflow; ACID stress harness is trivially passing; `import/export/benchmark` are print stubs; server always `init()`s (no `open()`), risking on-disk clobber. |

**Finding counts (Critical / High / Med / Low):** Storage-IO 6/12/12/7 · WAL-ACID
9/10/7/4 · Indexing 7/13/12/6 · Txn-MVCC 6/10/9/4 · Graph-DB 8/9/9/4 ·
Cypher-frontend ~11/19/14/6 · Cypher-backend-TCK 9/8/7/2 · Server-runtime
6/12/13/12. **Total ≈ 290 findings + ≈ 62 cross-cluster integration gaps.**
The full structured finding lists (with `file:line` evidence) are retained in the
audit run; Critical/High items are enumerated per cluster below.

---

## 3. Cross-cutting integration matrix (the decisive issue)

These wiring gaps are why the crate does not function as a database even though
most parts pass their unit tests. Each is the spine of Sprint A/B/C.

1. Checksums (`storage/page.rs`) ⟂ buffer-pool/storage I/O — no verify/update on read/write.
2. PageManager direct I/O ⟂ BufferPool — two cache-incoherent paths to the same pages; `free_page` doesn't invalidate the pool.
3. Flusher `flushed_lsn` ⟂ WAL durable LSN — WAL-before-data ordering is self-referential and unsound.
4. ARIES / double-write / group-commit / checkpoint ⟂ startup & commit — all dead code; redo-only recovery used instead.
5. Graph mutations ⟂ recovery — logical WAL records (NodeInsert/…) written but never replayed.
6. Properties ⟂ storage — high-level `Graph` and Cypher `CREATE` never persist or read properties.
7. Adjacency `link/unlink` ⟂ `put_edge`/`delete_edge` — never called; traversal impossible.
8. Secondary/RDF indexes ⟂ buffer pool + recovery — RAM-only; lost on restart; not rebuilt.
9. Txn/MVCC/lock/wound-wait ⟂ engine & server — zero callers; server uses last-writer-wins.
10. Index mutations ⟂ WAL durability — applied before commit is durable; no redo/undo.
11. CLI/gRPC ⟂ planner→physical→storage — both use the RETURN-literal-only executor.
12. Logical→physical lowering ⟂ operators — Expand/Merge inputs discarded; Apply/HashJoin/NodeByIdScan lowered to `AllNodesScan` stubs.
13. Planner label ids (XxHash64 u64) ⟂ stored label ids (u32) — no shared catalog; label scans never match.
14. TCK harness ⟂ parser/executor/state — never runs scenarios; compliance unmeasured.
15. logos lexer ⟂ parser — lexer dead; parser uses a weaker char scanner.
16. ConnectionAcceptor / RequestDispatcher / TLS / shutdown signal ⟂ tonic serve — bound then bypassed.
17. `GraphMode` (LPG/RDF) ⟂ engine — never threaded into storage.
18. `id.rs` UUIDv7 allocator ⟂ graph ids — never used; ids caller-supplied.

---

## 4. Remediation roadmap — sprints by magnitude of gains

Ordering principle: **highest gain first = the work that unblocks the most
downstream value and is most foundational.** Each task enumerates the findings it
resolves (by cluster + severity) so that **all ~290 findings and ~62 integration
gaps are covered**. Sprints execute sequentially; tasks within a sprint may
parallelise where independent.

- **Sprint A — Foundational Integration & Durability Spine** (highest gain):
  turn disconnected layers into one coherent, durable storage spine. Without it
  nothing else is reliable. Covers integration gaps 1-8,17-18 and the Storage-IO
  + Graph-DB durability findings.
- **Sprint B — ACID Correctness: Recovery, MVCC, Transactions** (non-negotiable
  ACID): real Atomicity/Isolation/Durability — ARIES with UNDO, MVCC integration,
  conflict detection, double-write/group-commit/checkpoint, and a *real* ACID
  test battery. Covers WAL-ACID + Txn-MVCC findings and gaps 3-5,9-10.
- **Sprint C — openCypher Execution Engine End-to-End** (non-negotiable
  openCypher): wire CLI/server→planner→physical→storage, implement the stub
  operators and missing clauses/expressions/types/semantics, and make the TCK
  harness actually measure compliance. Covers Cypher-frontend + Cypher-backend +
  gaps 11-15.
- **Sprint D — Concurrency, Indexing Robustness & Performance** (high-concurrency
  mandate): B+ tree structural correctness + real latch crabbing + variable-length
  keys + durable secondary indexes; buffer-pool concurrency soundness (loom/miri);
  remove the global engine mutex. Covers Indexing + Storage-IO concurrency findings.
- **Sprint E — Server Productionization & Hardening**: wire acceptor/dispatcher/
  TLS/backpressure/graceful-shutdown; real gRPC query+txn services; remove
  panics; metrics; CLI import/export/benchmark; RDF model. Covers Server-runtime
  + gap 16 + RDF.
- **Sprint F — Quality, Docs Accuracy & Test Depth**: fix the broken benchmark
  and clippy/dead-code; correct overstated module docs; crash/fault-injection +
  loom/miri + e2e + perf-regression test suites; config & error-model hardening.
  Covers all remaining Medium/Low findings and the Testing/Docs categories.

The concrete task breakdown lives in the `rmp` roadmap `rgraph` (Sprints created
from this audit). This document is the source registry; tasks reference it.

---

## 5. Reliability gate (definition of done for "usable with reliability")

The crate may be considered reliable for first real use when, **empirically**:
1. `cargo check --all-targets`, `cargo clippy -- -D warnings`, and the full test
   suite are green in CI.
2. A node/edge with properties can be created, the process killed mid-write
   (fault injection), reopened, and **all** data + properties + adjacency recover
   correctly (ARIES undo of losers verified).
3. Concurrent transactional workloads show **zero** lost updates / dirty /
   non-repeatable / phantom anomalies, validated by reading the durable engine
   (not an in-process mirror), with the declared isolation level.
4. The openCypher TCK harness runs real scenarios end-to-end through
   parser→planner→executor→storage and reports a tracked, rising pass rate
   (target 100%).
5. The gRPC server executes real graph queries and transactions under TLS, with
   backpressure/timeouts and graceful shutdown, with no panics on adversarial
   input, validated by end-to-end tests and a performance baseline.
