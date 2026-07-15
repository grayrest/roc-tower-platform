# Table-scoped in-memory response cache

**Date:** 2026-07-15 · seeded DB (5000 articles) · macOS aarch64. Follows the
materialized-aggregate SQL tuning (`2026-07-15-sql-materialize.md`).

## What it is

An outermost tower layer that caches already-compressed response bytes keyed on
`(method, path+query, Accept-Encoding)`. A hit returns finished bytes and never
reaches compression, the router, the Roc handler, or the DB. Opt-in is two-part:
the app enables `ResponseCache` (a setting), and each cacheable response declares
itself via `Res.cacheable(resp, tables)`.

- **Config (explicit, not implicit):** `ResponseCache({ ttl_ms, max_entries,
  methods, bypass_headers })`. Only `methods` are cached; any request carrying a
  `bypass_headers` header (`authorization`) skips the cache, so a personalized
  response never enters a shared entry.
- **Placement:** compiled OUTSIDE compression, so entries store the encoded body
  (`Accept-Encoding` is part of the key).
- **ETag:** the body is hashed once at insert; `If-None-Match` is answered `304`.
- **Invalidation (the platform's payoff):** every mutation is a declarative
  `DbWrite` through the one effect executor. After a write commits, effect.rs
  parses the target table and purges the cache entries that read it (reverse
  `table -> keys` index). TTL is only a backstop.

## Table dependencies

Each cached entry carries the set of tables its reads touched:
- **Generated readers** — sqlgen's VDBE analyzer already resolves every cursor to
  its base table (correlated subqueries land in the same program), so it emits
  `<query>_tables` beside each reader. Authoritative, zero runtime cost.
- **Dynamic reads** (the list/feed `Sql.query!` path) — declared by hand; over-
  declaring is safe (only over-purges).
- **Materialized-column caveat:** the by-slug read splices the trigger-maintained
  `articles.tag_list` column, so its physical read is `articles` only — sqlgen
  can't see that a tag edit (write to `article_tags`) cascades through the
  trigger. The app adds `article_tags` to the by-slug dep set explicitly. A fully
  general fix would use SQLite's `update_hook` to capture trigger-cascaded writes
  (rusqlite `hooks` feature; turso lacks an equivalent) — deferred, since one
  hand-declared dep covers the only materialized read we cache.

## Correctness

A scripted proof (`scratchpad/cache-check.sh`) — on BOTH engines — shows: an
anonymous read caches; a matching `If-None-Match` returns `304`; an authenticated
write of a new tag purges `/api/tags`; and editing an article's tags refreshes
the cached by-slug article (the materialized-`tag_list` edge). 154/154 hurl
conformance on rusqlite AND turso with the cache live.

## Throughput (wrk, 5000-row seed)

Cached anonymous GETs converge to the hit-path ceiling — the hit is a hashmap
lookup + serve-bytes, so it's endpoint-independent (~120–140k rps, p50 ~90µs).

| endpoint | materialized (no cache) | + cache | speedup |
|---|---:|---:|---:|
| **list** | 8,791 | **121,180** | **13.8×** |
| tags | 11,684 | 132,582 | 11.3× |
| comments | 32,349 | 120,544 | 3.7× |
| byslug | 62,490 | 120,367 | 1.9× |
| profile | 69,444 | 139,378 | 2.0× |

The ~120k ceiling is the tower stack + hyper, not the endpoint. Caching dwarfs
the SQL micro-opts for hot anonymous traffic — but those still matter for the
cache-miss/first-request path and for authenticated traffic (which bypasses the
cache by design).

## Reproducing

```sh
python3 bench/seed.py bench/seeded.db
just build examples/realworld/main.roc && cp main bench/srv-cache
bench/run.sh cache:bench/srv-cache
# correctness:
PORT=8770 ./main & scratchpad/cache-check.sh
```
