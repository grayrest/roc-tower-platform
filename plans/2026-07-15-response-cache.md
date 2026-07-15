# Plan: table-scoped in-memory response cache

> **DONE 2026-07-15.** All 7 phases shipped on `turso` (commits 5c08f22 infra,
> c858386 sqlgen deps + opt-in). Cached anonymous GETs hit-path ~120–140k rps
> (list 8.8k→121k, 13.8×); ETag/304 + table-scoped invalidation proven on BOTH
> engines incl. the materialized-tag_list edge; 154/154 rusqlite AND turso.
> Results: notes/2026-07-15-response-cache.md.

## Summary
An in-memory response cache as the outermost tower layer, opted into via a
`ResponseCache` app setting and a per-response `Res.cacheable(resp, tables)`
declaration. Entries are tagged with the tables their reads touched (sqlgen
emits the set for generated queries; dynamic reads declare it). Every `DbWrite`
in effect.rs purges entries depending on the written table — the platform's
single write choke point makes invalidation precise instead of TTL-guessing.
ETag falls out: hash the body once at insert, answer `If-None-Match` with `304`.

## Key architecture decisions (from the design discussion)
- **Config surface:** a global `ResponseCache({ ttl_ms, max_entries, methods,
  bypass_headers })` settings tag (matches the `Compression`/`ConcurrencyLimit`
  pattern). `bypass_headers` (e.g. `["authorization"]`) is the EXPLICIT auth
  skip — not implicit.
- **Placement:** `CacheLayer` is compiled OUTERMOST in `build_stack`, so it
  stores the already-compressed body; cache key includes `Accept-Encoding`.
  A hit returns finished bytes and re-runs nothing (router/Roc/DB all skipped).
- **Invalidation:** table-scoped, coupled in effect.rs. Reads' table
  dependencies ride to the cache on an internal `x-tp-cache-tables` header the
  outermost layer consumes + strips; writes parse their target table and purge
  the inverted `table -> keys` index. TTL is only a backstop for anything
  under-declared.
- **Opt-in is per-response AND global:** a response is cached iff the app
  declared the `ResponseCache` setting AND the response carries the
  `Res.cacheable` header AND method ∈ `methods` AND no `bypass_headers` present.

## Propagation channel (why headers, not an ABI change)
The Roc response crosses the FFI as `{status, headers, body, effects}`. Rather
than widen that record (glue regen, touches every handler), the handler declares
cache intent with internal headers via a `Res` helper:
`x-tp-cache-tables: articles,users,...` (+ optional `x-tp-cache-ttl`). Headers
pass through the compression layer unchanged, so they reach the outermost cache
layer intact; the layer reads and strips them before sending. Fully
Roc-declared, zero response-type churn.

## Impact
- Modify: `crates/host/src/config.rs` (HostConfig field + settings decode),
  `crates/host/src/middleware.rs` (CacheLayer, outermost), `crates/host/src/effect.rs`
  (purge on write), `platform/main.roc` (settings union tag), `platform/Res.roc`
  (`cacheable` helper), `crates/sqlgen/src/{analyze,emit}.rs` (emit per-query
  `_tables`), `examples/realworld/*` (opt in), `crates/host/src/lib.rs` (mod cache).
- Create: `crates/host/src/cache.rs` (the store + inverted index + TTL/LRU),
  `notes/…-response-cache.md`.
- Risks: (a) under-declared deps → stale reads (mitigated: TTL backstop +
  over-approximate table sets are safe); (b) cache key must include everything
  that varies the body (path, query, Accept-Encoding) or it serves wrong bytes;
  (c) authenticated responses must never enter a shared entry (bypass_headers +
  per-response opt-in both gate this); (d) memory unbounded without max_entries
  eviction.

## Phases

Phase 1: cache store (host, no wiring) — files: `cache.rs`, `lib.rs`
  - `ResponseCacheConfig { ttl, max_entries, methods, bypass_headers }`.
  - `struct Entry { body: Bytes, etag: String, encoding: Option<HeaderValue>, expires: Instant }`.
  - `key = (method, path+query, accept-encoding)`; `HashMap<Key, Entry>` +
    inverted `HashMap<Table, HashSet<Key>>`; capacity eviction (LRU or FIFO).
  - `lookup`, `insert(key, entry, tables)`, `purge_table(db, table)`, TTL sweep.
  - unit tests: hit/miss, expiry, purge-by-table, capacity eviction, etag stable.

Phase 2: config + settings ABI — files: `config.rs`, `platform/main.roc`, glue
  - `ResponseCache({ ttl_ms, max_entries, methods, bypass_headers })` union tag;
    `RocSettingTag` decode into `HostConfig.response_cache: Option<…>`.
  - `just glue` regen; assert no other settings drift.

Phase 3: CacheLayer middleware — files: `middleware.rs`
  - Outermost `BoxCloneSyncService`. Request: if method ∈ methods and no
    bypass header, compute key; on hit → serve (or `304` on `If-None-Match`).
    Miss → pass down; on response, if `x-tp-cache-tables` present, hash body →
    ETag, insert, strip internal headers, add `ETag`.
  - Skip entirely when `config.response_cache` is None.

Phase 4: write invalidation — files: `effect.rs`, `cache.rs`
  - After a `DbWrite` txn commits (and each autocommit `Opt` write), extract the
    target table from the SQL (`INSERT INTO x` / `UPDATE x` / `DELETE FROM x`)
    and `cache::purge_table(db, table)`. Shared cache handle via a startup
    `OnceLock`/static (mirrors the sqlite pool statics).

Phase 5: sqlgen table deps — files: `analyze.rs`, `emit.rs`, `tests/codegen.rs`
  - Collect distinct `Cursor::Table`/`Index` names in `analyze` → `Analysis.tables`.
  - Emit `<query>_tables : List(Str) = [...]` beside each reader.
  - test: article_by_slug_q_tables == {articles,users,favorites,follows,article_tags}.

Phase 6: Res.cacheable + realworld opt-in — files: `platform/Res.roc`,
  `examples/realworld/{main,Db,Articles,Tags,Profiles,Comments}.roc`
  - `Res.cacheable(response, tables)` sets the internal headers.
  - Add `ResponseCache` to the app settings; mark the anonymous GET responses
    cacheable with their table lists (sqlgen constant for generated readers, a
    hand-declared set for the dynamic list/feed).

Phase 7: verify + bench — files: `notes/…-response-cache.md`
  - 154/154 both engines (correctness: a favorite/write must invalidate the
    list/byslug it affects — add a hurl step if not already covered).
  - wrk: expect anonymous `list`/`tags` to jump toward tens-of-thousands rps
    (near-all hits under a 1s+ TTL); measure hit-path p50.

## Testing strategy
- Per phase: `cargo test -p tower-platform-host` (cache unit tests), `-p sqlgen`.
- Correctness gate: 154/154 hurl on BOTH engines — specifically a write-then-read
  sequence proving invalidation (favorite an article → its list entry refreshes).
- Final: A/B cached-vs-uncached on `list`/`tags`/`byslug`.

## Assumptions
- [ASSUMPTION: the dynamic list/feed path declares its table set by hand in Roc]
  — sqlgen can't see runtime SQL; over-declaring is safe (only over-purges).
- [ASSUMPTION: write SQL is single-target and simple enough to extract the table
  with a token scan] — true for every realworld write (INSERT/UPDATE/DELETE one
  table); a parse miss falls back to purge-all-for-db.
