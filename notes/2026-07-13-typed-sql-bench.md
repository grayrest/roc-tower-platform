# Typed-SQL benchmarks

## Phase 2 microbench gate (host-side, decision 7)

Go/no-go before migrating anything: does the per-cell cursor FFI beat the
materializing `sqlite_exec!` path? Measured in `ffi_tests::microbench_cursor_vs_exec`
(`#[ignore]`d; run with `cargo test -p tower-platform-host microbench -- --ignored --nocapture`).

Workload: `article_select` (the worst query — 4 correlated subqueries incl.
COUNT/EXISTS/GROUP_CONCAT) over 20 seeded rows × 15 columns, 3000 iterations,
pool + statement cache warmed. Allocations counted via a process-global
counting allocator (Roc's allocator routes through `std::alloc`, so this counts
both Roc-side marshalling and Rust-side Vec/String).

| path          | ns/query | allocs/query |
|---------------|---------:|-------------:|
| `sqlite_exec!`|  ~123700 |          240 |
| cursor        |  ~110800 |          201 |
| ratio         |     0.90 |         0.84 |

**Verdict: PASS.** Cursor is at parity-or-better on time (0.90×) and allocates
fewer (0.84×). Gate satisfied; migration proceeds.

### What moved the numbers

Two findings from the review were load-bearing:

1. **Statement caching (finding 3).** A first cut used raw `sqlite3_prepare_v2`
   per `sql_begin!` — recompiling the 4-subquery statement each call put the
   cursor at 1.87×. Switching to a self-referential holder over rusqlite's
   `prepare_cached` (connection → cached statement → rows, boxed and dropped in
   reverse) reuses the compiled program: 1.87 → 1.64.

2. **Hot-path lock (finding 8).** With a `Mutex<HashMap>` registry lookup in
   every one of the ~320 per-query getter/step calls, the cursor stayed at
   1.64× — the lock, not SQLite, dominated. Making the handle BE the cursor
   pointer (sound because handles are non-escaping, decision 2) drops the hot
   path to a direct deref with no lock: 1.64 → 0.90. The registry is now touched
   only at begin/finish/sweep.

### Caveat

This host-side bench does not capture the Roc→host FFI crossing cost of ~320
getter calls per list query (the reviewer's finding 10) — those are direct
`extern "C"` symbol calls, cheap, but only the Phase 6 end-to-end HTTP
benchmark measures them in situ. The allocation win here (0.84×) is understated
too: integer columns (ids, counts, flags) allocate nothing on the cursor path
but this bench still stringifies none of them — the real generated readers skip
even more. See Phase 6 below.

## Phase 6 end-to-end (2026-07-14)

Full HTTP benchmark, `main` (materializing `sqlite_exec!`) vs the `typed-sql`
branch (cursor FFI + generated readers), **both built with the same fixed roc
compiler** (`fix-erased-callable-static-data-ambiguous-template`), same machine
(darwin/aarch64), back-to-back.

- **Data (deterministic seed, `seed.py`):** 200 users / 5000 articles / 10000
  comments / 9931 article-tags / 10117 favorites / 994 follows. Byte-identical
  schema between branches ⇒ one seeded DB drives both binaries.
- **Load:** `wrk -t4 -c16`, 3s warm + 15s measured per endpoint, two
  interleaved passes (main→branch each pass). Anonymous GETs only.
- **Endpoints:** `list` = `/api/articles?limit=20` (20 rows × 15 cols, 4
  correlated subqueries/row — the worst query); `byslug` =
  `/api/articles/article-2785` (1 row × 15 cols, generated reader); `profile` =
  `/api/profiles/user14` (1 row × 4 cols); `comments` =
  `/api/articles/article-2785/comments` (9 rows × 8 cols).

Averaged over both passes (rps, and p50 latency):

| endpoint | path shape | main rps | branch rps | Δ rps | main p50 | branch p50 |
|----------|-----------|---------:|-----------:|------:|---------:|----------:|
| list     | 20×15, dynamic `Sql.query!` | 6515 | 6521 | **+0.1%** | 1.74ms | 1.77ms |
| byslug   | 1×15, generated reader      | 50164 | 48234 | **−3.8%** | 271µs | 280µs |
| profile  | 1×4, generated reader       | 56646 | 57204 | **+1.0%** | 248µs | 249µs |
| comments | 9×8, generated reader       | 24543 | 24965 | **+1.7%** | 545µs | 538µs |

(Raw two-pass numbers in `scratchpad/bench/results.txt`; p99 is too noisy to
compare — list p99 swings ~100ms from allocator/scheduler spikes on both.)

### Verdict: gate NOT met as written — parity overall, one real regression

The decision-10 bar was "no endpoint regresses >2% AND article-list p50
measurably improves." Neither half holds:

1. **`byslug` regresses ~3.8%** — consistent across both passes (branch below
   main in every sample), so it is real, not noise.
2. **`list` p50 did not improve** — it is at parity (+0.1% rps, p50 flat within
   noise).

**Why, and why it's the *expected* shape of this trade.** The cursor path swaps
host-side row materialization (one structured RocList crossing per query, plus
Rust-side Vec/String allocs) for **per-cell FFI crossings** (a `sql_col_*!`
call per column, ± a `sql_col_type!` probe). That is exactly the cost the Phase
2 host microbench flagged as unmeasured (finding 10). Where it lands:

- **`byslug`** is the pure signal: a single 15-column row at ~50k rps. Almost
  no SQLite work, so the ~15+ getter crossings that replaced one materialized
  crossing dominate the delta → the regression concentrates here.
- **`list`** does the same per-cell crossings ×20 rows, but each row costs 4
  correlated subqueries; SQLite CPU swamps the crossing overhead → parity. The
  microbench's 0.90× (host-side, allocation-bound) and this end-to-end parity
  are consistent — the alloc win and the crossing loss roughly cancel in situ.
- **`profile`** (4 cols) and **`comments`** (IO/row-count bound) are at parity
  or a hair faster.

### Net

End-to-end throughput is **parity ±2% on three of four endpoints**, with a
real **~3.8% regression on the widest, cheapest, highest-QPS single-row read**.
The typed-SQL branch's payoff is **compile-time schema safety and single-source
SQL** (drift is a codegen/`roc check` error, not a runtime decode miss), not
raw throughput; on throughput it is a wash with a narrow, well-understood
regression. Closing the `byslug` gap would need the batched row-getter the plan
named as the perf-gate fallback — a new host FFI shape + emit change, deferred
as its own decision rather than smuggled into Phase 6.

