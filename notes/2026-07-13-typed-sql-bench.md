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

## Phase 6 end-to-end (pending)
