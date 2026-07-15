# Typed-read + Roc-serialize vs `@sql json` — end-to-end A/B (rusqlite)

**Date:** 2026-07-15 · **Build:** default rusqlite host · seeded DB
(`bench/seed.py`, 200 users / 5000 articles / 10000 comments) · macOS aarch64 ·
wrk `-t4 -c16 -d15s`, pooled median of 2 order-balanced passes (typed-first +
json-first, to cancel thermal/load drift).

The whole RealWorld app runs in BOTH modes (`Api.read_mode = Typed | Json`),
each passing **154/154** conformance. `Typed` reads typed rows and serializes
the envelope with Roc's `Json.to_str`; `Json` streams the SQL-assembled
document (`@sql json`) verbatim.

## Results

| endpoint | shape | typed rps | json rps | Δ rps | typed p50 | json p50 |
|----------|-------|----------:|---------:|------:|----------:|---------:|
| byslug   | 1-row single object          | 56925 | 55949 | **+1.7%** | 248µs | 248µs |
| profile  | 1-row single object          | 67253 | 68273 | **−1.5%** | 214µs | 210µs |
| comments | multi-row aggregate, **sqlgen positional** read | 30110 | 19880 | **+51%** | 456µs | 710µs |
| tags     | flat aggregate array         | 11367 | 11438 | **−0.6%** | 1.04ms | 1.01ms |
| list     | 20-row aggregate, **dynamic by-name** read | 5079 | 6150 | **−17%** | 2.66ms | 2.03ms |

## Reading — the thesis holds where the typed read is cheap

The bet was: SQLite's JSON aggregate engine is the movable cost; get typed rows
into Roc cheaply and let Roc's serializer replace SQL-side JSON assembly. The
data splits cleanly along **how the rows are read**, which is exactly what the
thesis predicts:

- **Single objects (byslug, profile): parity.** No aggregate to eliminate, and
  one `json_object` ≈ one Roc record encode. Neither side has an edge (±2%).
- **`comments`: typed wins +51%.** This is the thesis in its pure form — a
  `json_group_array` aggregate replaced by a Roc-serialized list, where the row
  fetch is a **generated positional** read (`sqlgen`, `Host.sql_col_*` by index,
  no per-cell column lookup). SQLite's aggregate assembly was the cost; moving
  it to Roc removed it.
- **`tags`: parity.** Also an aggregate, but a trivial flat string array — there
  is essentially no assembly cost to reclaim, so it's a wash.
- **`list`: typed LOSES −17%.** *Also* a `json_group_array` aggregate, but the
  dynamic list/feed path can't go through sqlgen (its WHERE is built at runtime
  from filters), so it reads rows via `Sql.query!` **by name** —
  `find_first_index` over the ~12 column names for *every cell* of *every* row
  (≈240 lookups for a 20-row page), plus a `split_tags` per row. That per-cell
  lookup overhead more than eats the aggregate-elimination win.

**The list regression is not a counter-example to the thesis — it's the cost of
the read mechanism, not the serialize step.** `comments` and `list` both build
N records and Roc-serialize them; the only structural difference is positional
(sqlgen) vs by-name (`Sql.query!`) cell reads, and that is where the ~68% swing
between them comes from. A positional dynamic-read path (or a way to route the
list through sqlgen) would very likely flip `list` to a win too.

## Consequence for eliminating `@sql json`

- Eliminating `@sql json` is **a clear win on the heaviest aggregate endpoint
  (comments, +51%)** and **neutral on single objects and tags**.
- The one regression (`list`, −17%) is attributable to the platform's dynamic
  read being **by-name**, not to Roc serialization. Closing it needs a
  positional `Sql.Row` accessor (or sqlgen coverage of the filtered list) —
  tracked as the obvious follow-up before a full cutover.
- Net: the typed path is viable as the single API surface today, with one known
  optimization (positional dynamic reads) standing between it and a clean sweep.

## Turso (same A/B, `--features turso`)

Same seeded DB, same 2-pass order-balanced method, turso backend:

| endpoint | shape | typed rps | json rps | Δ rps | typed p50 | json p50 |
|----------|-------|----------:|---------:|------:|----------:|---------:|
| byslug   | 1-row single object | 15285 | 14281 | **+7.0%** | 0.91ms | 0.95ms |
| profile  | 1-row single object | 26857 | 25780 | **+4.2%** | 557µs | 580µs |
| comments | multi-row aggregate, positional | 11786 | 10704 | **+10.1%** | 1.26ms | 1.38ms |
| tags     | flat aggregate array | 3186 | 3054 | **+4.3%** | 3.52ms | 3.61ms |
| list     | 20-row aggregate, by-name | 2496 | 2689 | **−7.2%** | 4.66ms | 4.30ms |

### Reading — typed is a *modest, uniform* win on turso, not a dramatic one

Two things differ from rusqlite:

1. **Typed wins even the single-object endpoints (+4–7%).** rusqlite was at
   parity there; turso is not. This is turso's known `@sql json`-specific
   weakness (the B0 spike: ~3.75× at the SQL level) showing through even on a
   one-object `json_object` — Roc's encoder beats turso's JSON builder.
2. **The `comments` mega-win collapses from +51% to +10%.** Counterintuitive if
   turso's JSON engine is *the* weak spot — but the typed path reads every cell
   through turso's **cursor FFI** (`row_value` → owned `Value` → RocStr), and
   turso's per-cell extraction is itself costly at volume. So on turso, moving a
   multi-row aggregate from JSON-assembly to typed-read trades one slow turso
   path (JSON) for another (per-cell cursor reads) — still a net win, but a small
   one. On rusqlite the typed read is nearly free, so the full aggregate saving
   lands.

`list` still loses (−7%, same by-name `Sql.query!` overhead as on rusqlite,
here compounded by turso's per-cell cost).

### Why turso is 2–3.7× lower end-to-end when execution is at parity

The B0 "parity" number (0.97–1.04×) measured a **reused** prepared statement —
execution only. But the two backends cache statements differently in the real
server:

- `cursor.rs` (rusqlite) uses **`prepare_cached`** — the plan is reused across
  requests on the pooled connection.
- `turso_db.rs` uses **`prepare_single`** — every request re-parses and re-plans.

Measured on the 5000-row seed (turso-spike `measure_prepare_cost`, simple typed
5-col indexed read):

| path | per-query | prepare tax |
|------|----------:|------------:|
| turso, statement reused (the parity #) | 1.93µs | — |
| **turso, `prepare_single`/query (server path)** | **12.91µs** | **+10.98µs** |
| **rusqlite, `prepare_cached`/query (server path)** | **1.87µs** | ~0 (cache hit) |
| rusqlite, fresh `prepare`/query (no cache) | 3.70µs | +1.82µs |

So **execution really is at parity** (1.93 vs 1.87µs) — but in the server turso
pays a **+11µs parse+plan on every query** that rusqlite caches away, making the
per-query cost **6.9×**. Two compounding facts: turso's planner is ~6× slower to
plan than SQLite's (+10.98 vs +1.82µs), *and* `turso_db.rs` doesn't cache while
`cursor.rs` does. A request runs ≥2 queries (auth token lookup + the data
read(s)), so this is ~11–22µs/request — a large share of the end-to-end gap; the
remainder is turso's generally younger step/IO/cell-extraction path on the
heavier real queries.

**Actionable:** `turso_sdk_kit` exposes `prepare_cached`; switching
`turso_db.rs` from `prepare_single` to it (the pooled connection persists across
requests, so the cache would survive) should recover most of the gap. Not yet
tried.

### Both engines, one sentence

- **rusqlite:** typed is a *big* win on the heavy fast-read aggregate
  (comments +51%), parity on single objects/tags, −17% on the by-name list.
- **turso:** typed is a *small* win almost everywhere (+4–10%), −7% on the
  by-name list.
- The **only** endpoint that regresses on *either* engine is `list`, and its
  cause is the platform's by-name dynamic read — not Roc serialization. Fixing
  that (positional `Sql.Row` reads) is the one thing between the typed path and
  an unconditional win on both engines.

## Reproducing

```sh
python3 bench/seed.py bench/seeded.db                      # once, deterministic
# Api.read_mode = Typed
just build examples/realworld/main.roc && cp main bench/srv-typed
# Api.read_mode = Json
just build examples/realworld/main.roc && cp main bench/srv-json
bench/run.sh typed:bench/srv-typed json:bench/srv-json     # pass 1
bench/run.sh json:bench/srv-json typed:bench/srv-typed     # pass 2 (order-balanced)
```
