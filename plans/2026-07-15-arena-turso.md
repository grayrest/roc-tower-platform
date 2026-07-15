# Plan: Request arena + Turso migration

**Revision 2** — amended after a 4-lens independent adversarial review
(memory soundness, lifecycle/concurrency, Turso ground truth, process/gates).
Review summary at the bottom; every amendment below is tagged `[R:n]`.

Two sequential workstreams after `typed-sql` merges to `main`:
**A. `request-arena`** — bump allocator behind `roc_alloc`/`roc_dealloc`, region
freed when the last `Arc<RequestArena>` drops (ctx guard + in-flight response
body — deferred `Opt` effects hold no Roc data and need no arena reference [R:5]).
**B. `turso`** — spike-gated wholesale swap of the rusqlite runtime for Turso
Database, ending in an honest zero-copy assessment (B4) whose borrow mechanism
is now known to depend on *stored* doc columns, not computed JSON [R:10].

Decisions locked (2026-07-14): merge typed-sql first · `@sql json` stays uniform
(regression accepted) · wholesale cutover after the spike gate (sqlgen keeps
rusqlite for typing; turso becomes an acceptance linter in sqlgen-check [R:4]) ·
B runs through the zero-copy assessment + materialized-views spike ·
unpushed-local-compiler dependency **acknowledged, no action** (sole user;
fixes will be reported upstream) [R:0-dismissed].

---

## Workstream A: request-scoped arena (branch `request-arena`)

### Verified seams (corrected by review)
- Exported `roc_alloc(length, alignment)` has no host param (`lib.rs:61`) →
  current-arena is TLS. The app is **interpreted** (fresh `LirInterpreter` per
  entrypoint call, `interpreter_shim/main.zig:171`) and forwards every Roc-value
  allocation to host `roc_alloc` — no cross-call caching to worry about.
- Roc's refcount word and slice element-count word live **inside** the
  allocation (at and after the returned pointer); the host's size header before
  the pointer is touched only by the allocator itself — tagging it is safe.
- Whole request runs synchronously in one `spawn_blocking` closure
  (`router.rs:204`). **A panic unwinding out of a hosted fn skips Rust cleanups
  in those frames** (documented at `roc_call.rs:93-98`) — RAII in the blocking
  closure is NOT reliable; `ctx_guard` lives on the async side for exactly this
  reason [R:2].
- The real foreign-thread decref site is **`CtxGuard::drop` on the async
  reaper task** (memo-thunk release via `decref_erased_callable`, incl.
  recursive `on_drop` capture teardown — `effects.rs:392-414`). Deferred `Opt`
  effects carry only deep-copied Rust data (`effect.rs:77`) and never touch
  refcounts [R:5].
- Response body handoff (A3): the body owns its Roc memory via
  `Bytes::from_owner` (bytes 1.10 ✓); region released by the write's
  completion. `ctx_guard` still drops at request end — deferring it would hold
  DB connections across client I/O (the profiled hold-time bottleneck).

### Design (fixed, amended)
- **Placement**: arena lives in `crates/host/src/arena.rs` + the hand-written
  `lib.rs` exports. The generated `roc_platform_abi.rs` is **never edited** —
  `just glue` regenerates it whenever the roc binary or `platform/*.roc`
  changes and would silently wipe the edits [R:3]. `lib.rs` builds the
  `RocHost` fn-pointer table itself (instead of `make_roc_host` defaults) so
  host-side abi allocations route through the arena too.
- **Header layout**: identical to the existing scheme —
  `size_storage_bytes = max(alignment, 8)`, size in the last word of the
  header. 16-byte alignments are live (erased-callable payloads, 16-aligned
  list elements); getting this wrong silently truncates realloc copies [R:6].
  Arena tag = high bit of the size word; **the tag reflects where the bytes
  actually came from** (arena chunk vs global), in both alloc and realloc —
  a cap-boundary realloc that falls through to global writes a plain header
  [R:12].
- **TLS discipline**: the TLS slot owns an `Arc<RequestArena>` (never a raw
  pointer). Install = unconditional overwrite, dropping any stale Arc (a
  skipped drop from a panicked predecessor becomes a bounded, self-healing
  leak, not a dangling pointer). Uninstall = set `None`. No save/restore stack
  — arenas never nest [R:2].
- **`roc_realloc`**: tagged + TLS arena → bump new + copy (in-place growth
  when last allocation); tagged + no TLS → global alloc + copy, plain header;
  untagged → existing global realloc. Old user length recovered with the same
  `max(alignment, 8)` convention [R:6].
- **Concurrency spec**: `RequestArena` bump state is either atomic or the type
  is `!Sync` with allocation confined to the guard thread by API — stated, not
  assumed [R:13]. Debug builds assert on arena-pointer refcount underflow at
  dealloc (the no-op path would otherwise mask double-decref bugs that the
  global allocator crashes on today) [R:14].
- **Cap fallback**: past 4 MiB total, allocs/reallocs fall through to global
  with plain headers.
- **Escape audit**: model init pre-request (no TLS → global — correct); model
  immutable after init; response bytes ride the body owner (A3); memo thunks
  ride the ctx guard's Arc [R:5]. State crosses requests only via effects.

### Phases
**Phase A1: arena core** — files: `crates/host/src/arena.rs` (new),
`crates/host/src/lib.rs` — est. ~260 lines
- `RequestArena` (chunk list, bump offset, cap fallback, test-only
  chunk-alloc/free counters [R:15]) + TLS slot owning `Arc<RequestArena>` +
  overwrite-install guard [R:2]
- Arena-aware `roc_alloc/roc_dealloc/roc_realloc` in `lib.rs` exports +
  hand-built `RocHost` table; `DefaultAllocators` (generated) untouched [R:3]
- Unit tests: roundtrip; **align-16 alloc → realloc grow → contents
  preserved** (TLS and foreign-thread branches) [R:6]; tag-matches-source at
  the cap boundary [R:12]; foreign-thread dealloc no-op; **erased-callable
  teardown (`on_drop` decref'ing arena captures) from a no-TLS thread** [R:5];
  stale-TLS overwrite (install, simulate skipped drop, install again, old
  arena freed) [R:2]; debug underflow assertion fires on double-decref [R:14]

**Phase A2: request lifecycle wiring** — files: `crates/host/src/router.rs`,
`crates/host/src/effects.rs` — est. ~70 lines
- Mint `Arc<RequestArena>`; `CtxGuard` owns a clone and its Drop decrefs memo
  entries **before** releasing it (field order) [R:5]
- Inside the `spawn_blocking` closure: install TLS, run `call_handler` under
  `std::panic::catch_unwind`, clear TLS on every path, resume unwind —
  RAII-only cleanup is unreliable across Roc frames [R:2]
- **No** per-`Opt`-task wiring — deferred effects hold no Roc data [R:5]

**Phase A3: zero-copy response handoff** — files:
`crates/host/src/roc_call.rs`, `crates/host/src/body.rs`,
`crates/host/src/router.rs` — est. ~110 lines
- `RocBodyOwner { body: RocListWith<u8>, arena: Arc<RequestArena> }`,
  `AsRef<[u8]>`, `unsafe impl Send` (atomic refcounts + tag-based dealloc),
  **declared field drop order: body before arena** [R:13]
- **Neutralize the response's body field (overwrite with an empty list)
  before the recursive `response.decref`** — the generated decref
  unconditionally decrefs `body` and `RocListWith` is `Copy`, so a "move"
  alone double-frees (worst on the >4 MiB cap-fallback path: genuine double
  `std::alloc::dealloc`) [R:1]
- `ResponseParts.body` becomes `enum Body { Roc(RocBodyOwner), Copied(Vec<u8>) }`;
  a failing `Must` drops the `Roc` owner on the handler thread and returns
  `Copied` (err-handler replies stay copied) [R:7]
- Slow-drain bound: fall back to copy when `arena_total / body_len` exceeds a
  threshold or a process-wide in-flight-arena-bytes gauge passes its cap —
  N slow readers must not pin N full request arenas indefinitely [R:8]
- Tests: cap-fallback body (double-free canary) [R:1]; `Must`-failure
  replacement [R:7]; timeout fires while the blocking task runs (outcome +
  owner dropped on reaper thread, arena freed by last Arc) [R:9];
  aborted-connection mid-write cleanup

**Phase A4: gates + bench + docs** — files: `bench/` (new, committed),
`crates/host/src/ffi_tests.rs`, `notes/`, `README.md` — est. ~180 lines
- **Commit the bench harness**: `bench/seed.py` (deterministic seed), wrk
  driver script, methodology notes — A4's numbers and B3's "arena baseline"
  gate must be reproducible from the repo, not a session scratchpad [R:11]
- Leak test: chunk frees == chunk allocs after last Arc drop, covering both
  orderings (reaper drops first / body owner drops last) [R:15]
- Full gates: build zero-warnings, clippy, roc-test, `just sqlgen-check`,
  154/154 conformance
- 4-endpoint wrk bench (main vs arena) + note: does the arena flip the
  `@sql json` multi-row numbers? (documenting only)

---

## Workstream B: Turso migration (branch `turso`, after A lands)

Reviewer ground truth (positive): every SQL function the live queries use is
✅ in Turso's COMPAT.md (`json_object`, `json_patch`, `json_group_array`,
`json()`, `iif`, `COALESCE`, `GROUP_CONCAT`, correlated `EXISTS`, `:name`
params, LIMIT/OFFSET, `busy_timeout`, WAL, `stmt_readonly`); `turso_sdk_kit`
0.7.0's `async_io: false` blocking mode is real; `BEGIN IMMEDIATE` exists;
the materialized-views builder flag is real.

### Phases
**Phase B0: coverage + perf spike (GATE)** — files: `crates/turso-spike/`
(new, standalone) — est. ~260 lines
- Run realworld schema + every sqlgen query + `@sql json` queries + dynamic
  list/feed SQL; **flag-correctness assertion**: run list/feed with a viewer
  who favorites/follows and one who doesn't, assert the flags differ (mixed
  named+positional binding is a silent-wrong-data risk) [R:16]
- **Writer-vs-writer contention**: conn A holds a write txn; conn B (with
  busy_timeout) issues `BEGIN IMMEDIATE` — assert it *waits*. Turso documents
  BUSY classes that never invoke the busy handler; if it fails fast, B1 needs
  a host-side retry loop or single-writer mutex [R:17]
- **Durability + file compat**: kill -9 mid-transaction → reopen →
  `PRAGMA integrity_check`; verify rusqlite can open a turso-written DB (the
  rollback path) [R:18]
- **Pragma matrix**: `synchronous=NORMAL` is NOT supported (OFF/FULL only) —
  choose the replacement and document the durability/latency trade [R:19]
- **Constraint-error classification**: assert turso surfaces extended codes
  distinguishing UNIQUE vs PRIMARY KEY violations (`classify_db_error`,
  `effect.rs:285-290`, feeds conformance-exercised 422s) [R:20]
- **Crate choice documented**: sdk-kit's README says app users should prefer
  the `turso` crate; record why rsapi's sync mode wins anyway, pin the exact
  version, name the fallback (async `turso` + block_on, or `turso_core`) [R:21]
- Microbench vs rusqlite (article_select workload)
- Materialized-views spike (report-only): can an incremental view express
  `article_json_summary`? This is now also **B4's dependency** [R:10]
- **Gate (pre-committed)**: no-go if any live query is unsupported/incorrect,
  if writer-contention has no workable mitigation, **or if the article_select
  microbench ratio exceeds 1.5× rusqlite** [R:22]

**Phase B1: pool + write effects on turso** — files:
`crates/host/src/effects.rs`, `crates/host/src/effect.rs`,
`crates/host/Cargo.toml`, workspace `Cargo.toml` — est. ~220 lines
- Own pool of turso connections (shape per B0 findings; busy/retry strategy
  per B0's contention result [R:17]); startup pragmas per B0's matrix [R:19]
- `effect.rs` migrates too (12 rusqlite references: `TransactionBehavior`,
  `types::Value` in struct fields, `params_from_iter`, `classify_db_error`
  extended-code matching) — it was wrongly assumed untouched [R:20]

**Phase B2: cursor FFI on turso** — files: `crates/host/src/cursor.rs`,
`crates/host/src/ffi_tests.rs` — est. ~180 lines
- `roc_sql_begin/step/col_*/finish` over turso statements; statement cache
  per B0; single-copy getter discipline kept; handle = cursor pointer

**Phase B3: cutover + gates + rusqlite deletion** — files:
`crates/host/Cargo.toml`, workspace `Cargo.toml`, `crates/sqlgen/src/lib.rs`,
`notes/`, docs — est. ~160 lines
- **Dialect gate**: sqlgen-check gains a turso acceptance pass — prepare every
  `@sql`/`@schema` string against turso after the rusqlite typing pass
  (~30 lines; closes the analyzer/runtime split permanently — without it a
  future `@sql` query using a SQLite-only feature passes every commit gate
  and 500s at runtime) [R:4]
- Delete rusqlite/r2d2 from host deps (sqlgen keeps its pin, comment updated:
  typing-analysis + acceptance-linting only)
- **Doc sweep** (SQLite claims that become false): `platform/Sql.roc:1,64,232,238`,
  `platform/RawJson.roc:7`, `README.md:95` (architecture diagram), `README.md:372`
  ("SQLite assembles the response in C"), `examples/realworld/README.md` [R:23]
- **Rollback recipe written into the bench note**: revert range + the
  B0-verified file-compat statement [R:18]
- **Gate**: full `just check` + 154/154 conformance + no endpoint regresses
  >5% vs the A4 committed baseline [R:11]

**Phase B4: zero-copy assessment (GATE, restructured)** — spike off B3
- Reviewer-corrected mechanism space [R:10]: **computed `@sql json` output
  never transits the pager's buffer pool** — it materializes in
  statement/register memory. Pinned-page borrows (the Turso-specific lever)
  apply only to *stored* TEXT/BLOB columns. And statement-buffer borrowing
  (a) requires holding statement + connection until the response body drops —
  across client I/O, the bottleneck A3 explicitly rejects [R:9].
- So B4 is an *assessment*, not a borrow implementation:
  1. Measure the residual copy cost on blob payloads post-arena (bounds the
     prize; if noise → done, arena copy is the floor — honest exit)
  2. If the prize is real, the sound zero-copy route is **stored doc columns**
     (materialized views per B0's spike report, or write-through doc columns)
     + pinned-page borrow with a headroom word — i.e. B4's output is a
     go/no-go **for the materialization follow-up**, not a borrow hack here
- **Gate**: written soundness + cost analysis; either "arena copy is the
  floor" or a scoped materialization+borrow follow-up plan

---

### Risks (updated)
- Turso beta correctness — B0 now tests durability, contention, and error
  classification, not just query acceptance [R:17,18,20]
- `synchronous=NORMAL` gap changes the write profile either way [R:19]
- sdk-kit is semver-0 and not the blessed app surface — version pinned,
  fallback named [R:21]
- Slow-drain arena pinning bounded by the A3 copy-fallback heuristic [R:8]
- Peak memory = request allocation held to region drop; 4 MiB cap fallback
- Panic-path TLS is overwrite-heal, not RAII — bounded leak per panic [R:2]

### Assumptions
- [ASSUMPTION: typed-sql merges to main unchanged before A starts]
- [ASSUMPTION: local roc compiler with both fixes stays available at
  ~/Repositories/roc; upstreaming is tracked outside this plan (user decision)]
- [ASSUMPTION: no app/platform code stores Roc values cross-request outside
  effects (re-audited in A2)]

### Status
- [ ] A1 arena core
- [ ] A2 lifecycle wiring
- [ ] A3 zero-copy response handoff
- [ ] A4 gates + bench harness + docs
- [ ] B0 turso spike (GATE)
- [ ] B1 pool + write effects (incl. effect.rs)
- [ ] B2 cursor FFI
- [ ] B3 cutover + dialect gate + docs
- [ ] B4 zero-copy assessment (GATE)

---

### Appendix: adversarial review record (2026-07-14)
Four independent reviewers (memory soundness, lifecycle/concurrency, Turso
ground truth, process/gates). 24 raw findings → deduped, author-verified
against source before adoption. Blockers: response-body double-decref
(found independently by two reviewers), panic-skips-RAII in the blocking
frame (documented in-code, two reviewers), arena edits in a build-regenerated
file, analyzer/runtime dialect split. Key correction: deferred `Opt` effects
hold no Roc data — the foreign-thread decref site is the ctx guard's memo
teardown on the async reaper. Key inversion: zero-copy borrowing depends on
materialization (stored columns), not vice versa. Dismissed by user:
unpushed-compiler-dependency finding (sole user, fixes to be upstreamed).
