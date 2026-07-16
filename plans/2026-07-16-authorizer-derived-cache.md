# Plan: authorizer-derived response-cache dependencies

Decision log (the why, the review arc, all accepted trade-offs):
`notes/2026-07-16-derived-cache-decision-log.md`.

> **Round-4 independent adversarial review, 2026-07-16 (4 reviewers: authorizer
> feasibility, concurrency/lifecycle, caching semantics, worth-it/effort).
> Verdict: BUILD WITH REVISIONS.** No fatal to the architecture — the
> load-bearing assumption (prepare-time authorizer callbacks fire through
> trigger sub-programs, trigger name in `AuthContext.accessor`) was **verified
> in the pinned sources** (rusqlite 0.32.1 `hooks/mod.rs:405`; bundled SQLite
> 3.46.0 `codeRowTrigger` sets the auth context at sqlite3.c:152974). But the
> plan as first written is unsound in three ways and dead in one; build only
> with the revisions below. Three findings were independently converged on by
> 2–3 reviewers each (marked ×N).
>
> **Soundness revisions (before the Phase 5 surface deletion):**
> - **R1 — spec-compliant shared cache** (semantics reviewer, FATAL as specced):
>   handlers read request headers as pure record fields — invisible to key,
>   deps, and poison — so automatic caching serves one client's
>   header-variant response to another (cookies; cache poisoning). Fix: honor
>   response `Vary` in key matching; treat `Cache-Control: private` (not just
>   `no-store`) as store-suppressing; hard-default `cookie` + `authorization`
>   into `bypass_headers` (config adds, never removes); never cache a request
>   with a non-empty body. This is the RFC 9111 origin contract every
>   CDN/Varnish deployment already imposes — header-variance-without-`Vary`
>   becomes the app's bug, not a platform hole.
> - **R2 — read-path writes purge, not just poison (×2):** `sql_begin` accepts
>   DML; `ReadSet.wrote` making only *this* response uncacheable leaves every
>   existing entry on that table silently stale. On `wrote`, feed the derived
>   write actions to `purge_write`. (Rejecting non-readonly statements at
>   `ctx != 0` is the eventual posture, blocked on hello/e2e's mid-request
>   `migrate!` use → the deferred read-after-write design.)
> - **R3 — effectful responses poison (×2):** `run_effects` executes inside
>   `call_handler`, so a GET with a `Must DbWrite` stores its own pre-write
>   read after its own purge, and a later cache HIT skips the handler —
>   silently dropping its effects. Any `DbWrite`/`HttpSend` in the effects
>   list (or an `err_handler` rewrite) poisons the ctx; `Log` stays neutral.
>   The only true *regression* vs the shipped opt-in cache; one clause fixes it.
> - **R4 — runtime DDL flushes the analysis:** a `CREATE TRIGGER`/`ALTER TABLE`
>   carried in a `DbWrite` never reaches the shadow and the stale memoized
>   derivations keep passing. Schema-change authorizer actions in a write set
>   → execute the DDL on the shadow, clear both memos, `purge_all()`.
> - **R5 — shadow + memos keyed per database name** (feasibility F1): a global
>   shadow mixes multi-db DDL; a colliding `CREATE TABLE IF NOT EXISTS`
>   silently no-ops, dropping the second db's triggers → under-purge.
>   Realworld is single-db so no test here catches it.
> - **R6 — `PRAGMA foreign_keys=ON` on the shadow** (feasibility F2): FK
>   cascade writes are only compiled (hence reported) when the pragma is on;
>   unconditional ON is a sound over-approximation.
> - **R7 — nondeterministic SQL functions poison** (feasibility F3): the
>   authorizer reports `Function{name}` at prepare — denylist `random`,
>   `randomblob`, and the date/time family (args invisible → over-approximate).
>   Documented exclusion: bare `CURRENT_TIMESTAMP` is a keyword, fires no
>   callback — not catchable via the authorizer.
> - **R8 — poison `reserve_id!`/`reserve_runs!`** (process-global counters)
>   and the debug gauges; `env_var!` neutral rests on "host never setenvs
>   post-init" — document it.
>
> **Wiring corrections:**
> - **R9 — deps are taken INSIDE the blocking closure** right after
>   `call_handler`, threaded through the oneshot with the response parts. The
>   plan's "router, after the call" reads the ctx after the reaper dropped it
>   (router.rs:229–233) — deps always `None`, feature dead on arrival while
>   every poison test passes.
> - **R10 — no RAII `CURRENT_CTX` guard (×3):** a Roc panic skips drops in
>   intervening frames (router.rs's own documented model; why arena
>   install/uninstall is explicit). Set before `catch_unwind`, clear
>   unconditionally after, mirroring the arena.
> - **R11 — shadow derivation hygiene:** never `prepare_cached` on the shadow
>   (a cached hit fires zero callbacks → empty dep set memoized as truth);
>   NULL-column `SQLITE_READ` arrives as `AuthAction::Unknown`, and
>   `AuthAction` is non_exhaustive — the wildcard arm maps to `All`, never
>   ignore; the hook is `FnMut + Send + 'static` → collect via
>   `Arc<Mutex<…>>`, uninstall with the turbofish `None::<fn(...)>`.
> - **R12 — `migrate!` hardcodes ctx 0** (drop its ctx arg): `ctx == 0` as the
>   mirror trigger is app convention; a nonzero init ctx silently disables
>   caching forever.
>
> **Claim corrections (decision log amended to match):**
> - **R13 — D9 overclaimed:** the read-then-store vs write-then-purge race is
>   inherited from the shipped cache (identical path), TTL-bounded, and IS
>   silent staleness. Fix in Phase 3 with a per-table generation counter
>   (`record_read!` captures gens; `purge_write` bumps; `insert` drops the
>   entry if any gen moved) — also closes torn multi-statement reads.
> - **R14 — D11 understated ~3×:** derivation re-adds `follows` to profile and
>   comments too, and comments gain an `articles` dep — a follow flushes all
>   cached profiles + comment lists; an article INSERT flushes comment lists.
>   List/tags are *identical* to the hand lists (the anon variant genuinely
>   reads no favorites/follows). Recovery = anon variants of three queries.
>   cache-check must assert profile/comments behavior explicitly.
> - **R15 — cap the derivation memos** (value-spliced `Sql.query!` SQL grows
>   them unboundedly); don't memoize failed derivations.
> - **R16 — named bench gate:** miss-path A/B on profile (most
>   overhead-sensitive single-statement route) and list; merge gate ≥ baseline
>   − 3%, hit path within noise of ~120k. Phase 2 is the highest-risk phase
>   and realistically ~300+ lines (two engines + FFI unwind boundary), not 180.

## Summary

Replace the hand-declared cache dependency surface (`Res.cacheable`,
`Db.*_read_tables`, `x-tp-cache-tables`, sqlgen `_tables`, the
`write_target_table` string parser) with **derivation from the SQLite
authorizer against an analysis-shadow connection**: reads record exact
`(table, column)` touch sets as they resolve; writes derive their
trigger-expanded write sets; purge is the intersection. Caching becomes
automatic for any app that enables the `ResponseCache` setting — no
annotations, column-scoped, fail-closed.

## Architecture (one paragraph)

The host keeps one in-memory rusqlite connection (the **shadow**) holding the
app schema, mirrored from init-time (`ctx == 0`) statements. Derivation =
prepare a statement against the shadow with `Connection::authorizer`
installed and collect the callbacks: `Read{table, column}` per column read;
`Insert/Update/Delete` per write **including trigger-fired writes** (SQLite
compiles trigger programs into the statement; `accessor` names the trigger).
Results are memoized per SQL text, so the request hot path pays a HashMap hit
after warmup, on both engines (turso has no authorizer — the shadow is the
same for both). Read deps accumulate on the request ctx (`RequestState`);
nondeterministic host calls poison the ctx uncacheable via a thread-local
current-ctx (the handler runs synchronously on one blocking-pool thread,
router.rs:211). The router attaches `{deps, cacheable}` to the response as an
`http::Extensions` value; the cache middleware consumes it. `effect.rs`
purges by derived write set; on any shadow failure: read side → uncacheable,
write side → legacy table parse, else full flush (never silent staleness).

## Impact

- Modify: `Cargo.toml` (rusqlite `hooks` feature), `crates/host/src/lib.rs`,
  `effects.rs`, `cursor.rs`, `turso_db.rs`, `router.rs`, `cache.rs`,
  `middleware.rs`, `effect.rs`, `config.rs` (nothing structural — setting
  unchanged), `crates/sqlgen/src/emit.rs` (drop `_tables`), `platform/Res.roc`
  (drop `cacheable`), `examples/realworld/{Db,Articles,Comments,Profiles,Tags}.roc`,
  `bench/cache-check.sh`.
- Create: `crates/host/src/shadow.rs`.
- Delete (surface): `Res.cacheable`, `x-tp-cache-tables`, `Db.*_read_tables`,
  sqlgen `_tables` emission, `write_target_table` as primary (kept as fallback).
- Risks: see Risks section.

## Phases

### Phase 1: shadow analyzer core — files: `Cargo.toml`, `crates/host/src/shadow.rs` (new), `crates/host/src/lib.rs` — est. diffs: ~250 lines
1. Add `"hooks"` to the workspace rusqlite features (authorizer lives behind it).
2. `shadow.rs`: a global `OnceLock<Mutex<Connection>>` `:memory:` connection.
   - `mirror_schema!(sql)` — execute an init-time statement against the shadow
     (errors logged, non-fatal: a seed INSERT replays harmlessly; DDL is what
     matters).
   - `ReadSet { deps: HashMap<Box<str>, Cols>, wrote: bool }` where
     `Cols = All | Named(HashSet<Box<str>>)`. A `Read` callback with no usable
     column name → `All` for that table. Any write action observed while
     deriving a "read" → `wrote: true` (callers poison).
   - `WriteSet { writes: HashMap<Box<str>, Cols> }` — `Update{t,c}` →
     `Named`, `Insert{t}`/`Delete{t}` → `All`. Trigger-fired actions included
     automatically (that IS the cascade — no closure pass exists).
   - `read_set(sql) -> Result<Arc<ReadSet>, String>` /
     `write_set(sql) -> Result<Arc<WriteSet>, String>`: memoized
     (`RwLock<HashMap<Box<str>, …>>`), derive on miss by installing the
     authorizer, `prepare()`, uninstalling. Internal host SQL (the `counters`
     sequence) never routes through here.
3. Unit tests (mini schema: two tables + a favorites-style AFTER INSERT/DELETE
   trigger pair maintaining a materialized column):
   - read of the materialized column reports exactly `(t, col)`;
   - `INSERT INTO fav …` write set = `fav: All` ∪ `arts: Named{fav_count}`
     (proves trigger expansion + column scoping);
   - `SELECT count(*)` degrades to `All`; failed prepare returns `Err`;
   - memoization returns the same `Arc`.

### Phase 2: request attribution + poison — files: `crates/host/src/effects.rs`, `cursor.rs`, `turso_db.rs`, `router.rs` — est. diffs: ~180 lines
1. `effects.rs`: `RequestState` gains `deps: HashMap<Box<str>, Cols>` +
   `cacheable: bool` (default true). Add `record_read!(ctx, &ReadSet)`
   (unions deps; `wrote` → poison), `poison!(ctx)`,
   `take_cache_deps(ctx) -> Option<CacheDeps>` (called by the router before
   teardown). Thread-local `CURRENT_CTX: Cell<u64>` with an RAII setter.
2. Poison the nondeterministic primitives via `CURRENT_CTX`: `roc_now_ms`,
   `roc_random_bytes`, `roc_next_id`, `roc_http_send`, `password_hash`
   (random salt). `env_var!`/`log!`/`password_verify` stay neutral.
3. `cursor.rs` + `turso_db.rs` `sql_begin`: `ctx == 0` → `mirror_schema!`;
   `ctx != 0` → `read_set(sql)` and `record_read!`; `Err` → poison.
4. `router.rs`: wrap `call_handler` in the `CURRENT_CTX` guard; after the call,
   `take_cache_deps(ctx)` and insert into the response's `Extensions` (only
   when a cache is installed).
5. Tests: an in-process test in `effects.rs`/`ffi_tests.rs` asserting a minted
   ctx accumulates deps from a `sql_begin` and that `roc_now_ms` poisons it.

### Phase 3: column-scoped cache index — files: `crates/host/src/cache.rs` — est. diffs: ~150 lines
1. `Entry.tables: Vec<Box<str>>` → `deps: HashMap<Box<str>, Cols>`; the
   reverse index stays table-keyed (`by_table`), entries stay exact-key.
2. `purge_table(&str)` → `purge_write(table, &Cols)`: iterate the table's key
   set, kill an entry when the written cols intersect its read cols
   (`All` on either side always intersects). Keep a `purge_all()` (fallback
   flush) and the existing TTL/FIFO behavior.
3. Update/extend unit tests: column-disjoint write leaves the entry (a write
   to `articles.title` does not purge a `tags`-shaped entry that read only
   `articles.tag_list` — the precision this buys); `All` purges; reinsert,
   eviction, etag unchanged.

### Phase 4: wire-through — files: `crates/host/src/middleware.rs`, `effect.rs` — est. diffs: ~140 lines
1. `middleware.rs::maybe_store`: take `CacheDeps` from response extensions
   instead of the `x-tp-cache-tables` header (header path deleted). Store when:
   setting enabled + method/bypass rules pass (unchanged) + status 2xx +
   `cacheable` + a `Cache-Control: no-store` response header is absent (the
   standard opt-out replaces `Res.cacheable` opt-in). Empty deps = valid
   (TTL-only entry).
2. `effect.rs`: `purge_written_tables` derives via `shadow::write_set(sql)`
   and calls `purge_write` per (table, cols). Fallback chain on `Err`:
   `write_target_table(sql)` → `purge_write(t, All)`; both fail → `purge_all()`
   + `tracing::warn`. Existing parser tests stay (it's the fallback).
3. Tests: effect-level test with the mini schema proving a trigger-carried
   write purges an entry that only read the materialized column.

### Phase 5: surface deletion + app migration + verification — files: `crates/sqlgen/src/emit.rs`, `platform/Res.roc`, `examples/realworld/*.roc`, `bench/cache-check.sh` — est. diffs: ~200 lines (mostly deletions)
1. Delete sqlgen `_tables` emission + its codegen test; `just sqlgen` to
   regenerate `DbGen.roc`.
2. Delete `Res.cacheable`; strip the wrapper from `Articles/Comments/
   Profiles/Tags.roc`; delete the five `Db.*_read_tables` constants.
3. `bench/cache-check.sh`: keep every invalidation proof (tags write, bio,
   favorites-trigger, tag-edit-refreshes-by-slug — now *derived*, the point of
   the exercise); flip the "follow does NOT invalidate by-slug" assertion to
   "follow DOES invalidate" with a comment citing D11 (accepted; recoverable
   via an anon query variant, out of scope here).
4. Gates: `just check-examples`, `just test-roc`, `just lint`,
   `cargo test -p tower-platform-host`, 154/154 conformance on **both**
   engines, `bench/cache-check.sh` on both.
5. Bench snapshot vs `notes/2026-07-15-response-cache.md`: hit path should be
   unchanged (~120k rps list); miss path must show no regression beyond noise
   (derivation is memoized; the per-request cost is a HashMap union). Write a
   short `notes/2026-07-16-derived-cache.md` with results.

## Testing Strategy

- Per phase: unit tests named above; the build must stay green after each
  phase (each is independently shippable — Phases 1–2 are inert until Phase 4
  flips consumption).
- Final: full gate battery (5.4), cache-check.sh both engines, wrk A/B against
  the shipped-cache numbers, `just sqlgen-check` no drift.

## Risks

- **Authorizer semantics drift** (empty column names, `SELECT *` expansion,
  scalar-subquery attribution): mitigated by degrading to `All` whenever a
  callback is ambiguous — over-purge is sound (D3). Phase 1 tests pin the
  behaviors we rely on.
- **Shadow/live schema divergence** (turso accepting SQL the shadow rejects):
  mirror failures log loudly; a read whose derivation fails is uncacheable,
  a write falls back conservatively (D9) — divergence degrades hit rate,
  never correctness.
- **`follows` regression on by-slug** (D11): accepted; cache-check documents
  it; anon-variant recovery is a listed follow-up, not in scope.
- **Hot-path cost**: one mutex-guarded memo lookup per `sql_begin` + a small
  union per read. If the Phase 5 bench shows contention, swap the memo to
  `arc-swap`/sharded map — do not pre-optimize.
- **`hooks` feature**: compile-surface only; bundled SQLite already ships the
  authorizer.

## Assumptions

- [ASSUMPTION: preparing (not executing) against the shadow is sufficient to
  fire all authorizer callbacks, including trigger programs — verified by the
  round-3 reviewer for rusqlite 0.32.1; Phase 1 tests re-prove it in-repo.]
- [ASSUMPTION: `Cache-Control: no-store` as the opt-out replaces the
  `Res.cacheable` opt-in — acceptable since the `ResponseCache` setting plus
  `bypass_headers` remain the safety gates (D5, D10).]
- [ASSUMPTION: init-time seed writes (none in realworld) replaying into the
  shadow are harmless; apps that seed large data at init pay a one-time
  shadow-memory cost — acceptable for v0.1.]

## Explicitly out of scope (per decision log)

Multi-write/read-after-write handlers (D6), compiler touched-set API (D2),
row tokens/prefix sweep (D3), authed caching (D5), the anon by-slug query
variant (D11 follow-up).
