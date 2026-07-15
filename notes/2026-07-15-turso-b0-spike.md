# B0: Turso coverage + perf spike — **NO-GO (magnitude corrected)**

**Date:** 2026-07-15 · **Crate:** `crates/turso-spike` (excluded from the
workspace; run `cargo run --release --manifest-path crates/turso-spike/Cargo.toml`) ·
**Turso:** `turso_sdk_kit` 0.7.0 (`turso_core` 0.7.0), sync `async_io:false`
stepping mode · macOS aarch64.

> **Correction (2026-07-15):** the original figures below were run in a **debug
> build**. rusqlite's bundled SQLite is compiled `-O2` regardless of the Rust
> profile, so debug compares optimized C against unoptimized Rust and inflated
> the turso ratio ~3×. The honest **release** numbers are **~4.5–5.6× at the SQL
> microbench** (not 8–18×) and **1.75–4× end-to-end** on realistic data. The
> microbench now refuses to report in a debug build. Everything below is updated
> to the release figures.

## Verdict — the gap is `@sql json`-specific, not the engine

The headline correction: on **realistic data (5000 rows), a simple typed indexed
read is at parity with SQLite (0.97–1.04×)**. Turso is *not* broadly slower. The
entire gap is the **`@sql json`** query — `json_object`/`json_group_array`/
`json_patch` + correlated subqueries — where turso's young JSON/subquery
execution runs **~3.75×** rusqlite.

| release, 5000-row seed, turso_core-direct vs rusqlite | turso | rusqlite | ratio |
|---|--:|--:|--:|
| simple typed read (5 indexed columns) | 1.8 µs | 1.8 µs | **1.0× (parity)** |
| `@sql json` document read | 17.3 µs | 4.6 µs | **3.75×** |

So the earlier "~5×" was a *worst case on both axes*: the heavy JSON query on a
2-row table (pure fixed overhead). It is real for `@sql json`, but it does **not
generalize** — the typed path, which is what the platform's query API is really
about, is competitive today.

**Consequence:** a *wholesale* swap is still not justified while the realworld
GET endpoints all run on `@sql json` (that's the 1.75–4× end-to-end). But a
**typed-first** platform on turso would be at parity — which is exactly the
direction the "eliminate the json variant" question points. Turso is viable; the
one thing to keep off it is heavy SQL-side JSON assembly.

Ruled out as causes of the gap: debug build (was inflating ~3×, now release);
MVCC (experimental, off by default); the rsapi wrapper's per-op `Mutex` (~1% —
`turso_core`-direct is within noise of rsapi); LTO (~8%); encryption feature
(nil).

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

## The microbench (pre-committed gate: ≤ 1.5× rusqlite)

Both engines in-memory, same schema + seed, **prepared statement reused** (reset +
rebind per iteration on both sides — an engine-to-engine execution comparison,
no per-iteration parse/plan), warmed, `black_box`'d, 2000 iterations, **release**:

| query | turso | rusqlite | ratio |
|-------|------:|---------:|------:|
| article-by-slug (`@sql json`, the hot read) | ~14.6 µs/q | ~2.6 µs/q | **~5.6×** |
| typed indexed point lookup | ~0.8 µs/q | ~0.2 µs/q | **~4.5×** |

The point lookup is the tell: Turso is ~4.5× slower on the most trivial indexed
2-predicate query, so this is per-query *execution* overhead in the young Rust
engine, not a `json`/subquery-planner weakness. The `@sql json` query widens it
to ~5.6× as the gap compounds over the correlated subqueries.

**Build-config sensitivity (the correction):** the same bench in a **debug**
build read 17.7× / 8.4×, and an earlier debug run with per-iteration `prepare`
read ~79× — both artifacts of comparing optimized-C SQLite against unoptimized
Rust. Release closes ~⅔ of the apparent gap. Further config barely moves it:
`fat` LTO + `codegen-units=1` shaves ~8% (5.6→5.2×); disabling turso's default
`encryption`/`pure-rust-crypto` features changes nothing (they're no-ops without
a key). The residual ~4.5–5.6× is genuine implementation maturity.

Also note the microbench runs on the **tiny spike seed (~2 rows)**, so it
measures per-query *fixed* overhead where turso is worst; on realistic data the
gap compresses (see the end-to-end note: 1.75–4×).

## Why it still gates a wholesale swap

The platform's read path is SQLite-bound (the arena bench,
`2026-07-15-arena-bench.md`, confirmed host allocation is a thin slice). A
1.75–4× DB slowdown end-to-end is not fatal for many apps, but it is a real
regression against the platform's fast-response headline, with no host-side
mitigation for raw engine speed — so a *wholesale* cutover is not justified on
current perf. It is a "not yet," not a "never": turso is pre-1.0 and closing the
gap release-over-release.

## Consequences for the plan

- **No *wholesale* cutover** on current perf — rusqlite stays the default host
  runtime and sqlgen's analyzer (no dialect split).
- **B1+B2 were nonetheless built** afterward (user's call) as a typed-API-on-Turso
  artifact behind `--features turso` — see `2026-07-15-turso-typed-endtoend.md`
  (154/154 conformance, 1.75–4× end-to-end). This corrected-magnitude view is
  what makes that build interesting rather than pointless.
- **Re-runnable:** the gate is a standing crate (`--release`). Re-run it against
  a future Turso release; if the ratio drops under ~1.5×, a default swap can be
  reconsidered.

## Materialized views (the B4-adjacent question)

Not separately spiked here: with Turso shelved, the zero-copy-borrow motivation
that drove the materialized-views question is gone. If read throughput on the
multi-row `@sql json` endpoints is later worth attacking, the
`2026-07-14`-measured **materialization** win (stored doc columns, 61–80% faster
reads at the pure-SQLite level) is achievable on *rusqlite* directly and is the
better-grounded lever — independent of Turso.
