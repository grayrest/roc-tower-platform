# B0: Turso coverage + perf spike — **NO-GO**

**Date:** 2026-07-15 · **Crate:** `crates/turso-spike` (excluded from the
workspace; run `cargo run --manifest-path crates/turso-spike/Cargo.toml`) ·
**Turso:** `turso_sdk_kit` 0.7.0 (`turso_core` 0.7.0), sync `async_io:false`
stepping mode · macOS aarch64.

## Verdict

**NO-GO on the wholesale rusqlite → Turso runtime swap.** The pre-committed gate
fails on performance: Turso 0.7.0 is **8–18× slower than bundled SQLite
(rusqlite)** on the RealWorld read path, versus a 1.5× ceiling. Everything else
Turso does well — the disqualifier is raw query speed, which is the one thing a
SQLite-assembled-response platform cannot give up.

## What passed (Turso 0.7.0 is genuinely capable)

| Check | Result |
|-------|--------|
| Query coverage | ✅ every RealWorld shape prepares + runs: `json_object`, `json_group_array`, `json_patch`, `json()`, `iif`, `COALESCE`, correlated `EXISTS`, nested `ORDER BY` subqueries, `LIMIT/OFFSET` |
| **Flag correctness** (the R:16 risk) | ✅ the mixed `:viewer` param reused 4× across two `EXISTS` subqueries binds to one slot and the favorited/following flags track the viewer exactly like SQLite |
| `tagList` sorted array, empty `{"comments":[]}` envelope | ✅ byte-identical output |
| Writer-vs-writer contention | ✅ 2nd `BEGIN IMMEDIATE` **honors `busy_timeout`** (blocked ~750ms then `Busy`), so a host retry isn't even required |
| Durability / file compat (rollback path) | ✅ rusqlite reopens a Turso-written file; `PRAGMA integrity_check = ok`; data intact |
| Pragma matrix | ✅ `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout`, and `synchronous=OFF/NORMAL/FULL` all accepted (the plan expected `NORMAL` unsupported — it is supported) |
| Constraint classification | ✅ UNIQUE and PK violations surface as `Constraint("UNIQUE constraint failed: users.email (19)")` / `...users.id (19)` — column-qualified, classifiable for the host's 422 mapping |

## What failed — the microbench (pre-committed gate: ≤ 1.5× rusqlite)

Both engines in-memory, same schema + seed, **prepared statement reused** (reset +
rebind per iteration on both sides — an engine-to-engine execution comparison,
no per-iteration parse/plan), warmed, `black_box`'d, 2000 iterations:

| query | turso | rusqlite | ratio |
|-------|------:|---------:|------:|
| article-by-slug (`@sql json`, the hot read) | ~150 µs/q | ~8.5 µs/q | **~17–18×** |
| typed indexed point lookup | ~14.5 µs/q | ~1.8 µs/q | **~8×** |

The point lookup is the tell: Turso is **8× slower on the most trivial indexed
2-predicate query**, so this is a *fundamental per-query execution overhead* in
the young Rust engine, not a `json`/subquery-planner weakness. The `@sql json`
query widens it to ~17× because the gap compounds over the correlated subqueries
SQLite's mature C executor eats cheaply.

An earlier measurement showed ~79× — that was ~520 µs/query of **per-iteration
`prepare`** (parse + plan of the large query), which the fixed bench removes by
reusing the prepared statement. Even fully amortized, execution alone is 8–18×.

## Why this is disqualifying

The platform's headline is that SQLite assembles the response document and the
host passes it through — the arena bench (`2026-07-15-arena-bench.md`) confirmed
these endpoints are **SQLite-bound**. Making the SQLite layer 8–18× slower would
dominate request time and regress every read endpoint by roughly that factor.
There is no host-side mitigation for raw engine speed; the arena/zero-copy work
saves microseconds on a path that would newly cost hundreds.

## Consequences for the plan

- **B1–B4 do not proceed.** The wholesale cutover, cursor-FFI-on-turso, dialect
  gate, and the B4 zero-copy DB borrows are all shelved: B4's borrow mechanism
  needed Turso's Rust-owned pager, and that lever is moot if the engine is 8×
  slower to begin with.
- **rusqlite stays** as the host runtime and sqlgen's analyzer — no dialect
  split to worry about.
- **Re-runnable:** the gate is a standing crate. Re-run it against a future
  Turso release; if the microbench ratio ever drops under ~1.5×, B1 can be
  reconsidered. Turso is pre-1.0 and improving fast, so this is a "not yet,"
  not a "never."

## Materialized views (the B4-adjacent question)

Not separately spiked here: with Turso shelved, the zero-copy-borrow motivation
that drove the materialized-views question is gone. If read throughput on the
multi-row `@sql json` endpoints is later worth attacking, the
`2026-07-14`-measured **materialization** win (stored doc columns, 61–80% faster
reads at the pure-SQLite level) is achievable on *rusqlite* directly and is the
better-grounded lever — independent of Turso.
