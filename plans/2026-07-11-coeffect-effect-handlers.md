# Plan: Coeffect/effect pure handler model

**Status:** 🟢 **READY FOR PHASE 1 (2026-07-11).** All gates passed: Phase-0
mechanics spikes, two independent adversarial reviews, and the Phase-0.5
integration re-spikes the second review demanded (I1 concurrent memo scoping,
I2 the 4-field return through the real routes path, I3 DB topology) — each with
evidence on the real request path, under concurrency, with leak checks. All
design decisions closed: D1 = host runs effects (Arch B); D2 = ctx-id-scoped
registry storing boxed thunks, author keys; DB = pool-for-everything. The
implementation phases below are now substantially *productization of proven
worktree code* (I2's worktree in particular already contains the 4-field
`requires`/`Res.*`/host-executor shape; I1's contains the ctx registry + guard).
Design log: [`notes/2026-07-11-coeffect-effect-handlers-design-log.md`](../notes/2026-07-11-coeffect-effect-handlers-design-log.md).

## Second review (2026-07-11) — integration gaps; reopen before building

A second reviewer audited the actual spike worktree code (not just the plan's
summary) and found the "all proven / ready to implement" status over-stated. The
spikes are real but each proved an *isolated* harness:

- **B1 (BLOCKING) — memoization is unsafe at concurrency, not "a lifecycle
  detail."** The memo spike used a **process-global** store (`Ctx := []`, a
  `&'static Mutex<HashMap<String, usize>>` keyed by author string only). Under the
  64–128 concurrent requests the platform targets, two concurrent `POST /users`
  both call `Ctx.get!("reserved_id", …)` → request 2 **hits request 1's box** →
  **same reserved id** (the exact double-reservation the memo exists to prevent),
  and the collision branch `decref`s a box another request may still `unbox` →
  **use-after-free**. So "D2 resolved" is **reopened**: the mechanism (opaque-box
  crossing, incref-on-hit, string key) is proven, but **per-request concurrent
  scoping is an unsolved design problem** — `Ctx := []` cannot carry a per-request
  handle, and the plan says the FFI argument stays `request` (nowhere to thread
  one). Candidate fixes (thread-local keyed to the `spawn_blocking` thread, with
  entry/exit reset + unwind-safe teardown; or thread a store handle through the
  FFI) are undesigned and unspiked.
- **B2 (BLOCKING) — Arch B forces ONE return type on ALL handlers; the "two
  dispatch paths" framing is a phantom.** The `routes` list and `requires` block
  pin a single handler return type (`main.roc`). Under Arch B that becomes the
  4-field `Response(…, effects)`, so **every** handler — including the escape
  hatch — returns 4-field, `Res.*` returns 4-field, every example changes.
  Coexistence works only by *unifying* onto the new type (the escape hatch returns
  `effects: []`). This contradicts the plan's Assumption ("escape-hatch dispatch
  unchanged") and Risk 7 ("two dispatch paths") — corrected below.
- **B3 (BLOCKING) — "run `Opt` post-flush" has no hook in the shipped server.**
  `call_handler` runs in `spawn_blocking`, returns `ResponseParts`, and `dispatch`
  hands a fully-buffered body up the tower stack; hyper writes it to the socket
  *later*, in the connection task. No host code runs after flush. `Opt` must
  either run before return (latency, not post-flush) or on a detached
  `tokio::spawn` (concurrent with flush, no ordering) — the stated "after flush,
  in order" guarantee is unachievable and must be redefined. Also: under Arch B
  the transaction + `err_handler` now run inside the tower `Timeout` window, so a
  408 can fire while the `spawn_blocking` write commits anyway (dropped
  `JoinHandle` ≠ cancelled blocking work).

**Over-claims corrected:** the closure-invoke spike used a **side-channel
entrypoint** (`spike_for_host`, its own `provides` slot) whose `requires` still
declared the OLD 3-field response and whose route table was never exercised — so
the 4-field `Response`-with-`effects` as the **actual handler return** through the
real `routes`→boxed-callable→`dispatch` path is **unproven**. The pool spike is a
standalone rusqlite binary, not wired to the host, and did not test the **mixed
topology** the plan ships (shared-`Mutex<Connection>` coeffect reads + pooled
`BEGIN IMMEDIATE` writes to the same file — a `SQLITE_BUSY` surface no spike hit).

**New gate — three integration re-spikes required before Phase 1 (Phase 0.5):**
(1) concurrent per-request memo scoping — ≥2 concurrent requests on the same key
must get **distinct** reservations, with unwind-safe teardown; (2) the 4-field
`Response`-with-`effects` through the **real** `routes` path (not a side channel),
extracted and executed in `dispatch`; (3) the mixed shared-read + pooled-write DB
topology under concurrency. Also decide: effects execute **inside** the handler's
`spawn_blocking` (S2) — coeffect resolution and effect execution must share one
execution context, or a thread-local memo store is gone by effect time.

## Spike outcomes (Phase 0 — PASSED, 2026-07-11)

Three spikes ran in isolated worktrees; all green. Combined with the adversarial
review's compiler checks, this proves the full mechanic set.

- **Memo / D2 — PASS.** Host-backed memoized resolution works: a non-idempotent
  coeffect resolved twice ran its injector exactly once (`runs=1`, stable over 200
  rapid repeats); a heterogeneous store held a `U64` and a `Str` under different
  keys at once. A polymorphic `Box(a)` in a *hosted* signature glues to an opaque
  `RocBox` (no per-type monomorphization); the host stores it as a raw pointer
  keyed by string, with `incref`-on-hit / keep-ref-on-put making repeat hits safe.
  **D2 resolved:** author-supplied `key : Str` per coeffect (identity-keying tested
  and disqualified — Roc can't produce the key, capturing closures collide, ICF is
  not guaranteed); the unchecked `Box.unbox` downcast is bounded by a
  **registration-time collision check** (store a cheap type discriminant, panic on
  mismatch). **One new must-build:** per-request store lifecycle (create/teardown,
  release retained boxes at request end) — the spike used a process-global store
  that leaks and cross-request-poisons; this lands in Phase 2.
- **Closure invoke / Spike 5 — PASS.** The host extracts the `RocErasedCallable`
  from a `Must` payload and invokes it; three distinct `Err` values → 409/404/408
  (genuine computation, not a constant), and the `Must` tag data is read host-side.
  Refcount teardown balances (6 alloc / 6 dealloc → 0 live; a skip-teardown
  negative control leaves 2 live and fails, so the check isn't vacuous). The
  generated `MustOrOpt::decref` already frees the Str + erased callable. Invoke
  shape mirrors `roc_call.rs:100`; content-hashed names (`MustOrOpt`,
  `NotFoundOrTimeoutOrUniqueViolation`, the response struct) get `types.rs` aliases
  when this graduates. `Box.box` on the `err_handler` is mandatory (unboxed →
  uninvocable `*mut c_void`, confirmed).
- **Connection pool / Spike 4 — PASS (and the shipped design is confirmed
  broken).** Under 64 concurrent requests the shipped single-shared-`Connection`
  cache produced 52 "transaction within a transaction" errors **and 3 leaked rows
  from rolled-back transactions** (atomicity violation). A pool (one connection
  checked out per request, its own `BEGIN IMMEDIATE…COMMIT/ROLLBACK`) passed all
  atomicity/isolation asserts. **Fix:** `HashMap<String, r2d2::Pool>` + per-request
  transaction API; PRAGMAs (`WAL`, `busy_timeout=5s`, `synchronous=NORMAL`) via
  `with_init`; `BEGIN IMMEDIATE`; pool size ~8–16. **Throughput reality:** SQLite
  serializes writers regardless of pool size (many readers, one writer); the pool
  buys correctness + read concurrency, not write parallelism, so the write-lock
  window must stay tight — which reads-before/writes-after gives *for free* (the
  host transaction wraps only the `Must` DB writes, never handler computation).

## Review outcome (adversarial pass, 2026-07-11) — verdict: GO-WITH-CHANGES

An independent reviewer ran the compiler and read the host source. **Confirmed
feasible (better than the risk register feared):** compiler-enforced purity (a
`Host.*!` call inside a `->` handler is a build error); an effectful `=>` wrapper
closing over and boxing a pure `->` handler type-checks; effectful static
dispatch through a memoized `Ctx.get!` type-checks; and — the key surprise — a
returned `List(Effect)` carrying **boxed** `err_handler` closures **glues cleanly**,
reusing the existing `RocErasedCallable` invoke/drop machinery. rusqlite exposes
the extended error codes needed for `UniqueViolation`/`ForeignKeyViolation`/`Busy`.

**Two unresolved DESIGN decisions, promoted to Gate 0:**

- **D1 — who runs effects, and does the FFI response type change?** The plan
  originally assumed *both* "FFI response unchanged" *and* "host runs the effect
  list" — mutually exclusive (a 4-field return does not unify with the shipped
  closed 3-field `RocResponse`, verified). Resolve to one:
  - **Arch A** — the Roc *wrapper* runs effects (calls `Host.*!` per effect,
    invokes `err_handler` in Roc), returns the plain 3-field record. FFI response
    unchanged; closures never cross. But "`Opt` after response flush" needs host
    cooperation (the wrapper returns before flush), and transaction scoping still
    needs a host txn API.
  - **Arch B** — the handler's *return* carries the effect list to the host; the
    host runs them. FFI response struct + `requires` + glue **change** (verified
    glue-feasible). `err_handler` **must** be `Box.box(|err| …)` — an unboxed
    lambda glues to an uninvocable `*mut c_void`.
  **DECIDED (2026-07-11): Arch B** — the host runs effects; the handler return
  carries the effect list; FFI response struct + `requires` + glue change;
  `err_handler` is boxed. Chosen for `Opt`-after-flush + host-managed
  transactions.
- **D2 — memo key + value safety. RESOLVED (2026-07-11) by spike I1** (which
  also corrected the first memo spike's storage design). Final shape: author
  `key : Str` per coeffect; per-request scoping via a host-minted `ctx : U64`
  threaded through the request record into a registry with an async-side Drop
  guard; values stored as boxed **thunks** (`Box(({} => a))`) — NOT raw `Box(a)`,
  which is memory-unsound to free from the host (variable header/alignment;
  the first memo spike only ever tested `Box(U64)` and over-claimed the general
  case). Proven under 64-way concurrency with clean Err/panic teardown. See I1
  in Phase 0.5.

**Corrections applied below from the review:** Assumption 1 rewritten (D1); Phase
3 now requires a connection **pool** not a shared-cache checkout (Risk 2 was
fatal as written); Spike 4 gains a concurrent case; **Spike 5** added (host-side
invoke+refcount of a round-tripped boxed `err_handler`, Arch B); S1 phase-ordering
fixed (the `Res.*` return-type change breaks every example until the wrapper
exists, so it lands *with* Phase 4, not Phase 1); S3 noted (effectful injectors
are not `expect`-testable — coeffect-layer coverage moves to e2e). `!` naming is
mandatory throughout: `inject!`, `Ctx.get!`, `where [a.inject! : …]`.

## Summary

Add a second, sanctioned handler style to the platform: **pure `->` handlers**
that take positional typed **coeffects** and return
`Response(http_code, headers, body, List(Effect))`. Coeffects are resolved
before the handler by effectful injectors through a per-request memoized
resolver `Ctx`; effects are executed after the handler by a host-side runner
(DB writes transactional and first, external writes after commit, fire-and-forget
last). The existing effectful `Request => Response` style is retained as a marked
escape hatch. The shipped argsN/extractor/`Res` layer *evolves* into the coeffect
layer rather than being replaced.

The work is intertwined Roc (platform modules: response/effect types, `Ctx`,
`inject` methods, the wrapper) and Rust host (per-request memo store keyed by
injector identity, DB-transaction lifecycle, effect execution, error
classification). Because three of the four load-bearing mechanics are unproven
against this compiler/FFI, **Phase 0 is a hard gate** — if a spike fails, the
dependent phase's design is revisited before any production code is written.

## Impact

**Modify:**
- `platform/main.roc` — the FFI-boxed handler *argument* stays `request`; under
  Arch B (D1) its *return* changes from the 3-field record to one carrying
  `effects: List(Effect)`, so `requires` + glue change. Add exposure/import of new
  modules (`Effect`, `Coeffect`/`Ctx`).
- `platform/Req.roc` — `from_request : Request -> Try(a, Response)` becomes
  `inject : Ctx => Try(a, Response)` on every extractor type (`Body`, `BodyStr`,
  `JsonBody`, `FormBody`, `QueryParams`, `Headers`, `Model`). Add the `argsN`
  successor (coeffect wrapper) that runs the inject → handler → effect pipeline.
- `platform/Res.roc` — helpers return the new `Response(code, headers, body,
  effects)` with `effects: []`.
- `crates/host/src/` — new per-request coeffect memo store + effect executor +
  DB-transaction lifecycle + error classification; the router's per-request
  dispatch calls the new pipeline for pure-style routes.
- `examples/hello/main.roc`, `examples/e2e/main.roc` — migrate most handlers to
  pure; keep one effectful escape-hatch handler.
- `README.md` — document the pure model as the sanctioned style; mark the
  effectful form as the escape hatch.

**Create:**
- `platform/Effect.roc` (or a section of an existing module) — `Effect : Must(Tag, err_handler) | Opt(Tag)`, closed `Tag : [DbWrite(...), HttpSend(...), Log(...)]`, and the per-effect `Err` unions (`[UniqueViolation(Str), ForeignKeyViolation(Str), Busy, Other(Str)]`, etc.).
- `platform/Coeffect.roc` (or a section of `Req`) — the `Ctx` type with `Ctx.request` and `Ctx.get`.
- `crates/host/src/coeffect.rs` — per-request registry `Mutex<HashMap<u64, RequestState>>` keyed by a host-minted `ctx : U64` threaded through the request record; memo map stores boxed THUNKS (never raw `Box(a)` — unsound to free host-side), `incref`-on-hit / keep-ref-on-put, unconditional displaced-pointer decref, registration collision check, Drop-guard teardown on the async side of `spawn_blocking`. (All proven by spike I1, worktree commit 0b6c8ba.)
- `crates/host/src/effect.rs` — effect executor: partition Must/Opt, per-file `r2d2` pool + per-request `BEGIN IMMEDIATE` transaction, invoke boxed `err_handler` via `RocErasedCallable` (shape per `roc_call.rs:100`), classify DB errors. `types.rs` aliases for the content-hashed `MustOrOpt`/err/response structs.
- `examples/pure/main.roc` — a from-scratch pure example (CRUD with reserved ids, transactional writes, 409 mapping).

**Risks (1–4 RESOLVED by the Phase-0 spikes; see "Spike outcomes"):**
1. ~~Injector-identity keying~~ **RESOLVED.** Identity keying disqualified;
   author `key : Str` + host opaque-box store proven. Residual: the unchecked
   downcast, mitigated by the registration collision check. Remaining build work:
   the per-request store lifecycle (leak/poison if omitted).
2. ~~`Ctx.get` dispatch + memoization~~ **RESOLVED.** Effectful `Ctx.get!` +
   host-backed memo threading proven (`runs=1` under 200 repeats; heterogeneous
   store holds mixed types).
3. ~~Purity boundary~~ **RESOLVED** (adversarial review): pure-`->`-inside-`=>`
   composition type-checks; a `Host.*!` inside a `->` handler is a build error.
4. ~~Per-request DB transaction~~ **RESOLVED, and the shipped cache is confirmed
   broken** (atomicity violation under concurrency). Requires an `r2d2` pool +
   per-request `BEGIN IMMEDIATE` transaction API (not a shared-connection
   checkout). SQLite serializes writers regardless of pool size — keep the
   write-lock window tight (reads-before/writes-after does this automatically).
5. **Error classification portability.** Mapping SQLite (and later Postgres)
   error codes into `[UniqueViolation, ForeignKeyViolation, Busy, Other]`.
   SQLite exposes extended result codes (`SQLITE_CONSTRAINT_UNIQUE` = 2067,
   `SQLITE_CONSTRAINT_FOREIGNKEY` = 787, `SQLITE_BUSY` = 5); `rusqlite` surfaces
   these. Risk is low for SQLite, deferred for Postgres.
6. **Non-idempotent coeffect correctness.** Id reservation (`nextval` / SQLite
   counter bump) must resolve exactly once per request. If memoization is subtly
   broken (Spike 1/2), ids double-reserve silently. Mitigation: a dedicated test
   asserting a reserved-id coeffect requested by two dependents yields one id.
7. **~~Escape-hatch coexistence (two paths)~~ CORRECTED (B2).** There are NOT two
   dispatch paths. Under Arch B there is ONE return type (4-field
   `Response(…, effects)`) and one FFI slot; the escape-hatch handler returns the
   same type with `effects: []` and does its I/O inline. The router's
   effect-execution step must no-op when `effects == []` so the escape hatch
   behaves as it does today. Real risk: every existing handler + `Res.*` must move
   to the 4-field return (not "unchanged").

## Phases

### Phase 0 — Spikes (HARD GATE) — ✅ PASSED 2026-07-11
**Files:** ran in three isolated worktrees; nothing merged (proofs, not code)
**Est. diff:** throwaway; not merged

All spikes green — see "Spike outcomes" at the top. Reference implementations
live in the spike worktrees (memo store + `Ctx.roc`; `spike.rs` closure-invoke;
`scratch-pool-spike`). The real phases below draw on those approaches; the code
itself is not merged. Original gate text retained for the record:

- **Spike 1 — injector-identity memo store.** Minimal `Ctx` with two coeffect
  types where B's injector calls `Ctx.get(A)`; A's injector increments a
  host-side counter. Assert: requesting B (and A positionally) runs A's injector
  **once**. Proves the host can key a per-request store by injector identity and
  resolve reentrantly. If it fails, fall back to author `key : Str` and re-run.
- **Spike 2 — `Ctx.get` dispatch + memo threading.** Extend Spike 1 to a
  three-level chain (C→B→A) and two positional coeffects sharing A. Assert single
  resolution of A across the whole request.
- **Spike 3 — pure/`=>` composition.** A pure `->` handler wrapped by an
  effectful `=>` wrapper that runs an injector before and a (stub) effect after.
  Assert: it type-checks, and that adding a `Host.*!` call *inside* the `->`
  handler is a **compile error** (purity enforced).
- **Spike 4 — per-request DB transaction UNDER CONCURRENCY.** Host opens a
  transaction, runs two ordered writes, rolls back on the second failing. Assert
  single-threaded rollback works — **and** run two concurrent requests against
  the *same* DB file and assert their transactions don't interleave/corrupt. This
  is the real test: the shipped cache holds one shared `Connection` per file
  (effects.rs), so a naive checkout either serializes all writers or corrupts
  concurrent transactions. Spike must prove a **connection pool** (N connections
  per file) resolves it.
- **Spike 5 — boxed `err_handler` round-trip (Arch B only).** Round-trip a
  `List(Effect)` with a `Box.box(|err| …)` `err_handler` from a handler return,
  through glue, into the host; invoke the closure via `RocErasedCallable` and
  drop it without leaking. Glue-side is already verified feasible; this proves the
  host-side invoke + refcount half.

**Test verification:** each spike asserts its invariant via `roc test`/an e2e
probe. Document results (pass, or the fallback taken) in the design log before
proceeding. **Spikes 4 (concurrency) and the D2 memo key/value crossing are the
two most likely to force a design change — run them first.**

---

### Phase 0.5 — Integration re-spikes (NEW HARD GATE, from the second review)
**Files:** worktree spikes; not merged
**Est. diff:** throwaway

The Phase-0 spikes proved mechanics in isolation; these prove they compose
per-request under concurrency. All three must pass before Phase 1.

- **I1 — concurrent per-request memo scoping (B1, the gate).**
  **Mechanism DECIDED (2026-07-11): host-minted request id through the FFI.**
  `call_handler` mints a unique `u64` (`AtomicU64`), inserts a registry entry
  (`Mutex<HashMap<u64, RequestState>>`), writes the id into a new `ctx : U64`
  field on the request record, and holds a **Drop guard** whose teardown removes
  the entry and decrefs every retained box (runs on normal return and Rust
  panic-unwind; Roc `crash` is process-exit, out of scope). Memo hosted fns take
  the id (`memo_get! : U64, Str => …`). Chosen over a thread-local because
  isolation holds **by construction** (no thread-discipline invariant), and the
  same registry entry is the natural home for ALL per-request host state — the
  pooled DB connection/transaction (I3/S2) keys off the same id with the same
  teardown point. The `ctx` field rides the glue regen Arch B already requires.
  **✅ PASSED (2026-07-11, worktree commit 0b6c8ba) — all five asserts:** 64
  concurrent requests → 64 distinct reservations, 64 distinct ctx ids, injector
  ran exactly 64 times globally; `runs=1` within each request; registry drained +
  `live_boxes=0` after each of 5 further bursts (RSS flat: +128 KB over 320
  requests); mid-resolution `Err` rejection tears down clean; host-panic tears
  down clean (500 via JoinError, server keeps serving). The `ctx : U64` FFI
  change was fully mechanical (~5 min: one `requires` line, one `types.rs` hash
  alias, one `call_handler` field; `Req.roc`'s open records needed nothing).
  **Two evidence-forced corrections, now part of the design:**
  1. **Store boxed THUNKS, not raw `Box(a)` — raw value boxes are memory-unsound
     to free from the host.** Roc's allocation header is 1 word for plain
     payloads but 2 words when the payload contains refcounted values
     (`allocateWithRefcount` in the compiler builtins), and a correct free needs
     payload alignment + recursive child decref — all erased for an opaque
     `Box(a)`. `Box(U64)` happened to work; `Box(Str)` aborted in libmalloc at
     teardown. Fix: `memo_put! : U64, Str, Box(({} => a)) => {}` — `Ctx.get!`
     stores `Box.box(|{}| value)` on miss, unbox-then-call on hit. Erased
     callables have a FIXED layout plus a compiler-generated `on_drop` that
     releases captures, so the glue's `decref_erased_callable` is exactly sound
     (heterogeneous captures — U64, Str, Try — verified).
  2. **The Drop guard must live on the async side (`router.rs::dispatch`), not
     in `call_handler`.** A panic unwinding out of a hosted fn crosses the Roc
     frames to tokio's catch, but Rust cleanups in frames BELOW the Roc frames
     do NOT run (observed: guard-in-`call_handler` leaked one box after a
     panic). Guard is minted in `dispatch` outside `spawn_blocking` and dropped
     after the `.await` — covers normal completion, JoinError, and
     future-cancellation. A cancelled future's still-running blocking task doing
     `memo_put` on a torn-down ctx releases the thunk and logs loudly.
  Ownership gotcha: same-key re-registration must decref the displaced pointer
  UNCONDITIONALLY — an `if old != new` guard leaks a reference when old == new.
- **I2 — the 4-field return through the REAL routes path. ✅ PASSED
  (2026-07-11, worktree agent-af90218a4f751b25b).** The `requires` handler type
  moved to the 4-field shape, every handler in both examples migrated, and the
  host extracts/executes/tears-down effects inside `call_handler` on the real
  dispatch path — no side channel. Evidence: Must-succeeds → handler's response
  + effect counters; Must-fails → the boxed `err_handler`'s 409 replaces the
  handler's 201, with the effect's Str payload round-tripped into the body (real
  computation); all 13 pre-existing routes byte-identical with `effects: []`;
  **all 20 pre-existing e2e tests pass unchanged** (the B2 one-return-type
  claim, tested); ~5,800 requests incl. ~1,900 err_handler invocations with
  flat RSS (3.9→3.87 MB); full `just check` green (37 expects, 7 FFI, 24 e2e).
  **Churn (calibrates Phases 1/4/5):** hello migration was 6 lines (handlers
  already end in `Res.*`); `Res.roc` +37; host +150. `RocResponse` hash changed,
  `RocRequest` unchanged. **Surprises:** (1) the err_handler reply record
  content-hashes to the same struct as the HTTP-send Ok payload — the shared
  `RocResponse` alias had to split into `RocHttpOk`/`RocReply` (trap to keep);
  (2) teardown is SIMPLER than the closure spike: the generated record decref
  recursively frees the effects list incl. the erased callable — one
  `response.decref(host)` covers everything; (3) `Req.reject` must stay
  unannotated (platform sibling modules aren't importable for annotations);
  whole-record `==` expects were rewritten to field comparisons (records
  carrying `Box(fn)` lists don't support `==`).
- **I3 — mixed DB topology under concurrency (S1). ✅ PASSED (2026-07-11) —
  decision: pool-for-everything, retire the shared cache.** Ran through the
  ACTUAL host code (`execute_statement`/`open_connection`), 64 concurrent
  requests, ~25% forced rollbacks, WAL file, 3 runs per scenario. The mixed
  topology (shared-connection reservations + pooled `BEGIN IMMEDIATE` writes)
  was **fully correct with zero BUSY** — but **only by accident**: `effects.rs`
  sets no pragmas, yet rusqlite installs a **default 5000 ms busy_timeout on
  every `Connection::open`** (proven by pragma probe). Stripping it (A0/C0)
  produced 62–63/64 reservation BUSY failures — no corruption, but near-total
  request failure. So mixed survives on an undocumented-in-our-code library
  default, runs the shared conn at different pragmas than the pool (FULL vs
  NORMAL synchronous), and keeps two topologies alive → **route everything
  through the pool** for a single topology with explicit pragmas.
  Other findings: WAL is a property of the *file* (no journal-mode conflict
  regardless of open order); a request never holds two pooled connections
  (reservation conn returned before the txn checkout — but the executor must
  never resolve a coeffect lazily while the txn conn is checked out); under
  sustained 10 ms write txns, reservation tails hit ~1.3 s in BOTH topologies
  (WAL's single write lock, not the topology) — keep `Must` txns tight; pool 16
  handled 64-way with zero BUSY/timeouts (writers serialize regardless; 8–16 ≈
  the blocking-thread cap is right).

Also decide here (S2): effects execute **inside** the handler's `spawn_blocking`
so coeffect resolution and effect execution share one blocking-thread context.

---

### Phase 1 — Roc effect/response data model (types only)
**Files:** `platform/Effect.roc` (new)
**Est. diff:** ~150 lines new

- Define `Response(http_code, headers, body, List(Effect))` (nominal or
  structural per what the wrapper needs — decide from Spike 3).
- Define `Effect : Must(Tag, err_handler) | Opt(Tag)`, with `err_handler`
  **boxed** (`Box(Err -> (code, headers, body))` — an unboxed lambda glues to an
  uninvocable `*mut c_void`, per review S2). Closed `Tag : [DbWrite({db, sql, params}), HttpSend(request), Log(Str)]`.
- Define per-effect `Err` unions: `DbErr : [UniqueViolation(Str), ForeignKeyViolation(Str), Busy, Other(Str)]`, `HttpErr : [...]`.
- Inline `expect`s: construct a `Response` with a `Must`+boxed `err_handler` and
  an `Opt`; unbox and assert the `err_handler` maps `UniqueViolation` → 409.
- **Do NOT change `Res.*` yet** — the moment `Res.text` returns the 4-field
  `Response`, every example handler stops matching the shipped 3-field FFI slot
  and `just check` goes red until the wrapper exists (review S1). The `Res.*`
  swap moves to Phase 4, landing atomically with the wrapper + example migration.

**Test verification:** `roc test platform/main.roc` — new `Effect`/`Response`
expects pass; existing Res/Req expects and the FFI/e2e suite are **untouched**
(no `Res.*` change, no host change this phase).

---

### Phase 2 — Coeffect resolver + extractor migration
**Files:** `platform/Coeffect.roc` (new) or `platform/Req.roc`, `crates/host/src/coeffect.rs` (new), `crates/host/src/lib.rs`
**Est. diff:** ~250 lines
**Depends on:** Spike I1 (PASSED) — this phase productizes I1's worktree code
(commit 0b6c8ba: `platform/Ctx.roc`, registry section of `effects.rs`,
`router.rs` guard, `examples/ctxmemo`, `crates/e2e-tests/tests/ctx_memo.rs`).

- `ctx : U64` field on the request record (`requires` + `call_handler` +
  `types.rs` hash alias — mechanical, proven ~5 min).
- Define `Ctx` (wraps the request; exposes `.request` and the id) with the memo
  primitive `get! : ctx, Str, ({} => a) => a` (I1's proven shape). `Ctx.get!(T)`
  = `get!(ctx, T.key, |{}| T.inject!(ctx))` — each coeffect type supplies its
  unique associated `key : Str` (the D2 contract).
- Migrate every extractor: `from_request : Request -> Try(a, Response)` →
  `inject! : Ctx => Try(a, Response)` (effectful → `!` name required). Pure
  request-only ones (`Body`, `param`) ignore `Ctx.get!`; add one read-shaped
  coeffect (`ReservedId`) exercising `nextval`/counter reservation.
- Host per-request registry (per I1, all proven): `Mutex<HashMap<u64,
  RequestState>>`; memo map stores boxed **thunks** (`Box(({} => a))` →
  `RocErasedCallable`, fixed layout, sound `decref_erased_callable`) — NEVER raw
  `Box(a)` (memory-unsound free, see I1 correction 1); `incref`-on-hit /
  keep-ref-on-put; displaced-pointer decref is UNCONDITIONAL on re-registration;
  Drop guard minted in `dispatch` (async side) — NOT in `call_handler` (Roc
  frames swallow Rust unwind cleanups below them, I1 correction 2); a
  registration collision check (thunk discriminant) turning key collisions into
  loud errors.

**Test verification:** the existing `from_request` `expect`s in `Req.roc` **must
be dropped or converted to e2e** — an `inject!` is effectful and `expect` rejects
effectful calls (review S3), so the coeffect layer is no longer unit-testable in
`roc test`; its coverage moves to e2e. An e2e probe route using a memoized
non-idempotent `ReservedId` requested twice → asserts one reservation (Risk 6).
`just check` green.

---

### Phase 3 — Host effect executor + transaction + error classification
**Files:** `crates/host/src/effect.rs` (new), `crates/host/src/router.rs`, `crates/host/src/effects.rs`
**Est. diff:** ~400 lines (pool rework + effect executor + closure invoke +
classifier — the review's estimate correction).
**Depends on:** Spike 4 & Spike 5 (PASSED).

- **Connection pool — pool-for-everything (decided by spike I3):** replace the
  `Mutex<HashMap<String, Arc<Mutex<Connection>>>>` cache with
  `Mutex<HashMap<String, r2d2::Pool<SqliteConnectionManager>>>`. Pool per file:
  `max_size=16`, `with_init`: `busy_timeout(5s)`, `journal_mode=WAL`,
  `synchronous=NORMAL`. `execute_statement` (the `roc_sqlite_exec` path,
  including coeffect reads and the reservation `UPDATE…RETURNING`) checks out a
  connection per statement and returns it before returning — autocommit only;
  `BEGIN` is never valid through this path. The effect executor checks out its
  own connection for the per-request `BEGIN IMMEDIATE` txn strictly AFTER
  injection completes, so a request never holds two pooled connections. Add deps
  `r2d2`, `r2d2_sqlite`. (Spike I3: mixed shared-reads + pooled-writes was also
  correct at 64-way concurrency — zero BUSY, exact counts — but only because
  rusqlite installs a default 5 s busy_timeout on every connection;
  pool-for-everything is chosen for a single topology with explicit pragmas, not
  because mixed was observed broken.)
- **Execution context (S2):** effect execution runs **inside the handler's
  `spawn_blocking`**, not in async `dispatch` after the `.await`. (With the
  ctx-id registry from I1 the memo store no longer *requires* thread affinity —
  the surviving reasons are that blocking SQLite and the synchronous
  `err_handler` invoke must not run on an async executor thread, and the effect
  phase must complete before the response is synthesized.) `call_handler`
  extends to: resolve coeffects → invoke handler → run effects → synthesize
  `ResponseParts`. The I1 Drop guard stays in async `dispatch` (teardown), while
  effect *work* stays in the blocking closure. Executor rule from I3: never
  resolve a coeffect lazily while the request's txn connection is checked out.
- `effect.rs`: given a `List(Effect)`, partition DB `Must` / external `Must` /
  `Opt`; run DB `Must` first in one `BEGIN IMMEDIATE` transaction (ordered),
  commit; run external `Must` after commit; on first failure **invoke the boxed
  `err_handler`** via `RocErasedCallable` (invoke shape per `roc_call.rs:100` —
  proven by Spike 5, refcount-balanced) with the classified `Err`, use its
  `(code, headers, body)`; auto-log failures. **`Opt` semantics (B3, must be
  redefined):** the shipped server has no post-flush hook, so "run `Opt` after the
  response is flushed, in order" is unachievable. Pick one and state it: run `Opt`
  synchronously **before** returning `ResponseParts` (adds latency, keeps order),
  or detach via `tokio::spawn` (concurrent with flush, no ordering, owns its own
  closure teardown). Also note the `Timeout`-window interaction: a 408 can fire
  while a `spawn_blocking` transaction commits anyway.
- `types.rs`: alias the content-hashed generated names (`MustOrOpt`, the `DbErr`
  enum, the `err_handler` response struct) per repo convention. Teardown reuses
  the generated `MustOrOpt::decref` (already frees the erased callable).
- Error classification: map `rusqlite` extended codes → `DbErr` union
  (`SQLITE_CONSTRAINT_UNIQUE`/`_FOREIGNKEY`/`SQLITE_BUSY`), else `Other(Str)`.

**Test verification:** e2e routes: (a) a `Must` DbWrite that violates a UNIQUE
constraint → 409 via `err_handler`; (b) two `Must` writes, second fails → first
rolled back (query confirms absence); (c) an `Opt` Log fires without affecting
status. `cargo test -p tower-platform-host` for the classifier unit.

---

### Phase 4 — The coeffect wrapper (wire inject → handler → effects)
**Files:** `platform/Req.roc` (wrapper), `examples/pure/main.roc` (new)
**Est. diff:** ~200 lines
**Depends on:** Spike 3, Phases 1–3.

- Implement the `argsN` successor: resolves positional coeffects via `Ctx`
  (Phase 2), calls the pure `->` handler, hands the returned `List(Effect)` to
  the host executor (Phase 3). Arity family `1..5` as today.
- `examples/pure/main.roc`: a CRUD handler end-to-end — `POST /users` with a
  `ReservedId` coeffect, a `Must(DbWrite)` with a `UniqueViolation → 409`
  `err_handler`, and an `Opt(Log)`.

**Test verification:** e2e against `examples/pure`: create returns 201 with the
reserved id in the body; duplicate returns 409; the row exists after 201 and is
absent after a rolled-back failure. `just check` green.

---

### Phase 5 — Migrate examples/README + escape-hatch demo
**Files:** `examples/hello/main.roc`, `examples/e2e/main.roc`, `README.md`, `crates/e2e-tests/tests/e2e.rs`
**Est. diff:** ~250 lines
**Depends on:** Phase 4.

- Migrate most `hello`/`e2e` handlers to pure coeffect/effect form; keep one raw
  effectful `Request => Response` handler labelled as the escape hatch (e.g.
  `proxy!` or a streaming-ish route), with an e2e test proving it still works.
- README: pure model as the sanctioned style, effectful as escape hatch,
  coeffect/effect/`Ctx` reference, the `err_handler`/409 pattern.
- Full gate.

**Test verification:** `just check` — all `roc test` expects, FFI tests, and the
full e2e suite (pure routes + retained effectful escape-hatch route) green.
Clippy clean.

## Testing Strategy

**Per phase:** as listed above. The load-bearing correctness assertions are:
- **Single resolution of a non-idempotent coeffect** (Phase 2) — the memoization
  correctness guarantee.
- **Transaction rollback** (Phase 3) — two ordered writes, second fails, first
  absent.
- **Typed error → status** (Phase 3/4) — `UniqueViolation` → 409, not 500.
- **Purity is a compile error** (Spike 3) — a `Host.*!` call inside a `->`
  handler must fail to build; add a `// ROC-SHOULD-NOT-COMPILE` fixture check.

**Final (end-to-end):**
- `just check` green: `roc test` (all expects), `cargo test -p tower-platform-host`
  (FFI + classifier), `cargo test -p tower-platform-e2e` (pure + escape-hatch
  routes), clippy clean.
- A concurrency probe: N concurrent `POST /users` requests each reserve a
  *distinct* id (no double-reservation, no cross-request transaction bleed) —
  reuses the v0.1 100-way concurrency harness.

## Assumptions

- [ASSUMPTION (revised per review, D1): the handler's FFI *argument* stays
  `request`, but under Arch B the *return* gains `effects: List(Effect)`, so the
  FFI response struct, `requires`, and glue **do** change — Phase 1's earlier
  "FFI untouched" claim was wrong. Under Arch A the return stays 3-field and the
  Roc wrapper runs effects. This is Gate-0 decision D1; the phases below assume
  Arch B unless D1 chooses A.]
- [ASSUMPTION: the memo store lives host-side (agreed in design). Spike 1
  validates injector-identity keying; if it fails, the fallback is an
  author-supplied `key : Str` per coeffect, which changes the coeffect type
  boilerplate but not the pipeline.]
- [ASSUMPTION: DB `Must` effects target the same database as any coeffect reads
  in the request, so one per-request connection/transaction covers them. Cross-
  database `Must` writes in a single request are out of scope (no distributed
  transaction).]
- [ASSUMPTION: `rusqlite` surfaces extended result codes for constraint
  classification (it does, via `SqliteFailure(Error { extended_code, .. }, _)`).
  Postgres classification is deferred with the Postgres backend.]
- [ASSUMPTION: serial coeffect resolution is acceptable (agreed). No phase builds
  parallel resolution; declarative deps are not introduced.]
- [CORRECTED per B2: the escape-hatch path does NOT reuse v0.1 dispatch unchanged.
  Under Arch B its return type moves to the 4-field `Response(…, effects)` (with
  `effects: []`) and its `Res.*` calls change with everyone else's. There is one
  unified return type and one dispatch path; the effect-execution step no-ops on
  an empty effect list so the escape hatch's observable behavior is preserved.]
- [ASSUMPTION: Phases exceed the "≤5 files" planning guideline in places (Phase 2
  touches Roc + host together) because the coeffect mechanic is inherently a
  Roc↔host contract and can't be validated half-built. Each phase is still
  independently testable.]
