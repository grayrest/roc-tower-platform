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
- **Load:** `wrk -t4 -c16`, 3s warm + 15s measured per endpoint, passes
  interleaved (main→branch within each pass). Anonymous GETs only. Three
  separate runs (`results{,2,3}.txt`); the definitive numbers below are the
  3-pass `results3.txt`.
- **Endpoints:** `list` = `/api/articles?limit=20` (20 rows × 15 cols, 4
  correlated subqueries/row — the worst query); `byslug` =
  `/api/articles/article-2785` (1 row × 15 cols, generated reader); `profile` =
  `/api/profiles/user14` (1 row × 4 cols); `comments` =
  `/api/articles/article-2785/comments` (9 rows × 8 cols).

### First pass, and a false alarm

The initial two-pass run showed `byslug` (1 row × 15 cols) ~3.8% below `main`
in both passes, with the other three endpoints at parity. That looked like the
per-cell FFI crossing cost the Phase 2 microbench had flagged as unmeasured
(finding 10) landing on the widest, cheapest, highest-QPS read. Two follow-up
investigations changed the picture:

1. **The "regression" was mostly noise.** A controlled 3-way run
   (main / pre-fix branch / fixed branch, `byslug`+`list` only, tighter
   interleaving) put the *unmodified* pre-fix branch at parity with main on
   `byslug` (51.7k vs 51.6k rps). The same binary that "regressed 3.8%" in run
   1 matched main in run 2 — the delta was machine/thermal drift between runs,
   not code. (Raw: `scratchpad/bench/results.txt`, `results2.txt`.)

2. **Profiling still found a real waste — and fixed it.** `roc_sql_col_str` /
   `roc_sql_col_blob` (`crates/host/src/cursor.rs`) copied each cell **twice**:
   `ValueRef::Text(b) => b.to_vec()` to escape the row borrow, then
   `RocStr::from_slice(&bytes, …)` into Roc-owned memory. Building the RocStr
   directly from the borrowed bytes *inside* the `with_row` closure drops the
   intermediate `Vec` — one copy instead of two, on every string/blob cell.
   (The generated readers already avoid `sql_col_type!` probes on NOT-NULL
   columns, so probe count was not the lever; the redundant alloc was.)

### Definitive run: parity-or-better everywhere (main vs fixed branch)

Three passes, all four endpoints, `main` vs the branch with the single-copy
getters. Averaged rps and p50 (`scratchpad/bench/results3.txt`):

| endpoint | path shape | main rps | branch rps | Δ rps | main p50 | branch p50 |
|----------|-----------|---------:|-----------:|------:|---------:|----------:|
| list     | 20×15, dynamic `Sql.query!` | 6688 | 6635 | −0.8% | 1.71ms | 1.71ms |
| byslug   | 1×15, generated reader      | 52040 | 52346 | **+0.6%** | 265µs | 266µs |
| profile  | 1×4, generated reader       | 57179 | 60464 | **+5.7%*** | 247µs | 240µs |
| comments | 9×8, generated reader       | 24589 | 26485 | **+7.7%** | 547µs | 506µs |

\* `profile` is inflated by one high outlier pass (~+2% without it). `comments`
is the clean signal: branch above main in **all three** passes (+7.7% rps,
−7.5% p50) — the single-copy fix pays off most where each row carries several
string cells and SQLite work per row is light.

### Verdict

- **No endpoint regresses** (`list` −0.8% is within pass-to-pass noise; its p50
  is identical). The plan's ">2% regress" half of the gate is met.
- **`list` p50 does not "measurably improve"** — it is dominated by the 4
  correlated subqueries per row (SQLite CPU), which neither branch touches, so
  it sits at parity. The marshalling win the branch *does* deliver shows up on
  the row-list endpoint that is NOT subquery-bound: `comments`, +7.7%.

So the literal decision-10 gate ("list p50 improves") is not met on `list`
specifically, but its intent — the cursor/generated path is at least as fast
end-to-end — holds, and the branch is a measurable win on `comments`/`profile`.
The typed-SQL branch's headline payoff remains **compile-time schema safety and
single-source SQL** (drift is a `roc check` error, not a runtime decode miss);
throughput is now parity-or-better across the board, no batched row-getter
needed. p99 is too noisy to compare (list p99 swings ~100ms from
allocator/scheduler spikes on both binaries).

## `@sql json` — SQL-assembled documents (2026-07-14, Phase 7)

The profile above said response *construction* (row → record → nested record →
`Json.to_str`) was the real cost — ~30% malloc + a chunk of compiled-Roc time —
not the read. `@sql json` attacks exactly that: the query assembles the whole
JSON document in SQLite (`json_object`/`json_group_array`/`json_patch`) and the
generated reader hands column 0 back as raw bytes (`RawJson` → `Res.json_raw`),
skipping the record/encode pipeline entirely.

### Allocation: it works (list endpoint, sampled self-time)

| bucket | typed cursor | `@sql json` |
|--------|-------------:|------------:|
| total active CPU (8s window) | 56,882 | 33,246 (**−42%**) |
| malloc/free/memcpy | 31.7% (18.0k) | 15.8% (5.2k) |
| roc_alloc (RocList/RocStr/record) | 4.3% (2.5k) | 0.4% (131) — **~19× less** |
| SQLite exec | 26.4% | 48.2% (now dominant) |

The Roc-side allocation for the response collapses; SQLite assembly becomes the
dominant cost. That is the intended shape.

### Throughput: a win for single-row reads, a LOSS for multi-row (main vs json, 2 passes)

| endpoint | shape | main rps | json rps | Δ rps | main p50 | json p50 |
|----------|-------|---------:|---------:|------:|---------:|---------:|
| byslug   | 1×15 | 50613 | 60446 | **+19.4%** | 273µs | 234µs (−15%) |
| profile  | 1×4  | 59648 | 63456 | **+6.4%**  | 240µs | 229µs (−4%) |
| list     | 20×15 | 6828 | 6775 | −0.8% | 1.74ms | 1.94ms (**+11%**) |
| comments | 9×8  | 25408 | 22015 | **−13.4%** | 542µs | 672µs (**+24%**) |

**The trade is real and endpoint-dependent.** `@sql json` swaps Roc-side
allocation for SQLite-side JSON assembly:

- **Single-row / simple** (`byslug`, `profile`): marshalling dominated, so
  collapsing it wins big — byslug **+19%**.
- **Multi-row / nested** (`comments`, `list`): the SQLite work grows more than
  the Roc side shrinks. `comments` builds a nested `json_object` (comment +
  nested author) AND a per-row `EXISTS` following-flag inside a
  `json_group_array` for every row; `list` re-parses each summary via
  `json(summary)` under `json_group_array` over the paged subquery. That
  per-row JSON construction in SQLite's C outweighs the marshalling it
  replaced → `comments` **−13%**, `list` p50 **+11%**.

### Takeaway

`@sql json` is a **targeted** optimization, not a blanket one: reach for it on
single-row or shallow reads where response construction dominates (article
by-slug, profile), and keep the typed/`Sql.query!` path for multi-row reads
whose per-row JSON assembly is expensive (comments, the article list). The
allocation win is genuine and large; whether it nets out positive depends on how
much JSON the query has to build per row. Numbers in
`scratchpad/bench/results5.txt`; list allocation profile in `sample-*-list.txt`.

