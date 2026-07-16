# Spec: derived response-cache dependencies (no compiler API)

> **REJECTED by independent adversarial review, 2026-07-15 (round 2). Do not
> build as specced.** Three reviewers converged: on this workload wholesale
> derivation is a net *regression*, and parts are unsafe. Findings:
> - **Regresses the shipped `follows`-pruning.** The honest VDBE-derived by-slug
>   deps are `[articles, favorites, follows, users]`; Phase 1 dropped `follows` on
>   a *semantic* argument (anon `following` is constant-false) that a MAY-analysis
>   of SQL text can't reach. Deriving = re-adding `follows` = purge on every
>   follow. Derived list is simultaneously over- (`follows`) and under-
>   (`article_tags`) approximate, so the hand override can't be removed anyway —
>   the "no hand-declared deps" premise is unachievable here.
> - **Headline cascade win is already shipped** (materialized columns correct via
>   hand-added `favorites`/`article_tags`, proven by `bench/cache-check.sh`); net
>   new correctness from the trigger parser ≈ 0.
> - **§4 implicit cacheability + §6 token-in-key routes a credential into the
>   cache** — `GET /api/user` returns `{email, token}`, does a DB read → cacheable
>   with no author action; isolation would rest on a 64-bit non-crypto hash.
> - **§7 "sound via purity" is unenforced and already violated** — app code calls
>   `Host.now_ms!`/`random_bytes!` directly; `reserved_id!` is a nondeterministic
>   coeffect that *writes* via `Sql.query!`, dodging both the flag and the
>   invalidation choke point. Needs a capability type (the deferred compiler API).
> - **Row tokens are inert-or-unsound** against `cache.rs`'s exact-key purge
>   (`purge_table("articles")` never sweeps `articles:slug:*`); the trigger-cascade
>   closure has no implementation path (`write_target_table` sees `CREATE…` →
>   `None` → empty closure → stale); two hand-rolled SQL parsers fail silently
>   toward staleness.
>
> **Survivor (the one real win): a build-time cascade lint, not a runtime
> mechanism.** sqlgen computes the *backward* trigger-cascade closure at build
> time (it has the `## @schema` triggers + a real SQLite) and emits `_tables`
> already closure-expanded (a read of `articles` transitively depends on
> `favorites`/`article_tags`). Keep the hand-declared deps as *semantic overrides*
> (`follows` drop, annotated); lint at build time that the effective set ⊇
> derived-closure − declared-drops, so a query edit that joins a new table fails
> the build. Table-string-keyed, no runtime parser / ctx primitive / re-key, no
> token-in-key, no implicit cacheability — captures drift-safety + cascade
> correctness while keeping the `follows` precision. Spec below retained for
> context.

---


Replace the hand-declared cache dep sets (`Db.*_read_tables` + `Res.cacheable(…, tables)`
+ `write_target_table`-parses-one-table) with dependency tokens **derived** from
query analysis and made sound across trigger cascades and the dynamic-SQL path —
so invalidation is correct-by-derivation, not correct-by-author-vigilance. Uses
only the existing toolchain (sqlgen's VDBE analyzer + the host); needs no new
compiler feature. Keys stay on the incumbent's request-derived path (below); the
un-forgeable-arbitrary-handler key is explicitly out of scope until a compiler
touched-set API exists.

## Soundness principle

Everything is a **MAY (over-approximate) analysis**, which is safe in both
directions: over-approximating reads → over-*invalidate* (purge a bit too much);
over-approximating writes → same. Never miss a token; precision is optional.

## Token shapes

- **Collection token** `T` (a table name, e.g. `articles`) — a read that scans /
  range-scans / joins-on-data / reads via subquery depends on the whole table;
  any insert/update/delete to `T` invalidates it. This is the sound floor.
- **Row token** `T:col:v` (e.g. `article:slug:foo`) — a read that enters `T`
  by a UNIQUE/PK **equality seek on a bound param** depends only on that row.
  A precision add-on layered on top of the collection floor.

## Components

### 1. sqlgen emits per-read dep descriptors (replaces `_tables`)
From the VDBE plan sqlgen already builds (`analyze.rs` resolves each cursor to a
table/index), emit for each generated read a list of dep **descriptors**:
- default: a **collection** token for every base table the plan touches
  (correlated subqueries included — their cursors are in the same program);
- when a table is entered by a unique/pk equality seek whose key is a **bound
  param**, additionally emit a **row** descriptor `{table, col, param}` the
  reader fills from the param at runtime.
The reader records these into the request ctx via a new host primitive
(§4) instead of the handler hand-attaching `Res.cacheable(tables)`.

### 2. sqlgen trigger-cascade closure (write-side expansion)
sqlgen already applies `## @schema` to a real in-memory SQLite for analysis, so
it can also parse each `CREATE TRIGGER` body and record which tables each trigger
*writes* given a write to its table. Build the map `table -> {cascaded tables}`,
take the transitive closure, and emit it as data the host loads at startup. A
committed write to `X` then invalidates tokens for `closure(X)`, not just `X`.

This is what keeps materialized-column reads correct: the anonymous list reads
only `articles`+`users` (it selects the materialized `favorites_count`/`tag_list`
columns, not the source tables), so its *honest* derived deps are `{articles,
users}`. A `favorites` insert emits `{favorites}` ∪ `closure(favorites)` =
`{favorites, articles}` (the `trg_fav_insert` trigger writes `articles`) →
purges the list. No hand-added `favorites` dep, no silent staleness.

### 3. Dynamic-path dep derivation (host)
The list/feed `Sql.query!` path has no generated reader. The host parses the
`FROM`/`JOIN` table identifiers out of the runtime SQL string (the read-side twin
of `write_target_table`) → **collection** tokens, over-approximate. Records them
into the ctx like a generated read. (CTE/subquery tables are included by scanning
all `FROM`/`JOIN` occurrences, not just the top level — over-approximate is fine.)

### 4. ctx dep accumulation
A new host primitive `Host.cache_dep!(ctx, token)` (and a batched form) records a
read's tokens into the per-request ctx set as it resolves. Generated readers call
it with their descriptors; the dynamic path calls it with its parsed tables. On a
**miss**, the cache middleware reads the accumulated set off the ctx and tags the
stored entry with it. Replaces the `x-tp-cache-tables` header entirely.

### 5. Write invalidation (effect.rs)
After a `DbWrite` commits: `write_target_table(sql)` → expand by `closure()` →
emit collection tokens; where the write's `WHERE`/`VALUES` bind the row key,
also emit the row token(s). Purge the reverse index (unchanged mechanism).

### 6. Keys — unchanged from the incumbent (request-derived)
- **Default key** computed in Rust at the tower layer: route id + path params +
  query string + `Authorization`-token hash + negotiated `Accept-Encoding`.
  Correct for any handler that varies only on those — essentially every REST
  handler; zero authoring.
- **Optional authored keyfn** (boxed, per coeffect) for non-standard variance,
  composed by `withN`/`map`/`resolve`; primitive coeffects (`Req.param`,
  `Req.query_or`, `Auth.*`) self-report their contribution. `and_then` chains
  author a key continuation or fall to Uncacheable.
- The un-forgeable *derived* key is out of scope (needs the compiler touched-set
  API). This is a hit-rate/safety refinement, not a correctness gate — authored/
  default keys are already the incumbent's status quo.

### 7. Nondeterminism opt-out (sound via purity)
Handlers are pure, so their only inputs are the coeffects bound at `withN`, which
the platform sees. A recognized nondeterministic primitive (`Req.now!`, an RNG
read) carries an `Uncacheable` marker; binding one poisons the route's
cacheability. Complete **because** a pure handler cannot reach the clock/RNG
except through a coeffect — provided such host calls are only ever exposed through
flagged coeffects (a platform-primitive discipline, checkable).

## Reuse
`cache.rs` (the `token -> entries` reverse index already exists — re-point it off
table-name strings onto these tokens), the `effect.rs` write choke point, and the
sqlgen VDBE analyzer.

## Non-goals (explicit)
- Un-forgeable arbitrary-handler keys (compiler touched-set API).
- Full **row-level** cascade precision — a trigger cascade currently expands to
  the *collection* token of the cascaded table (`favorites` write → `articles`
  collection), so it over-purges by-slug entries of unrelated articles. Row-level
  cascade needs tracing `NEW.<col>` through the trigger body; deferred.
- Caching authenticated responses beyond what a token-in-the-key allows
  (fragments per token; correct but low cross-user hit rate).

## What this buys over the shipped cache
- **No hand-declared deps** — the `Db.*_read_tables` lists and the manual
  `article_tags` materialized-column supplement disappear; correctness stops
  depending on an author noticing a cascade.
- **Cascade-correct by construction** — the case that needed a hand-added dep
  (materialized `favorites_count`/`tag_list`) is derived from the triggers.
- **Dynamic path covered** without a hand list.
- Optional row-level precision for point reads (bounded by the cascade non-goal).
