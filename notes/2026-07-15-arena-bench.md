# Request-arena end-to-end benchmark (A4)

**Date:** 2026-07-15 · **Branch:** `request-arena` vs `main` (1abdeb9) ·
**Harness:** `bench/` (committed) · macOS aarch64.

## Method

One deterministic seed (`bench/seed.py`, `random.seed(1234)`: 200 users, 5000
articles, 10000 comments) drives two byte-identical server binaries built from
the same RealWorld app — one linked against `main`'s host, one against the
arena host. `wrk -t4 -c16 -d15s` after a 3s warm pass, four read endpoints.

**Order matters.** The first (cool-machine) passes favored the arena by
double-digit percentages; later passes reversed. The cause was measurement
order, not the arena: within a pass the two servers are measured back-to-back,
so monotonic thermal/load drift over a ~10-minute run penalizes whichever runs
second. The numbers below **pool 8 passes, 4 with `main` first and 4 with
`arena` first**, so linear drift cancels. Take single passes with a large grain
of salt — per-server run-to-run spread is ~15%.

## Result (median of 8 order-balanced passes)

| endpoint | shape | main rps | arena rps | Δ rps | main p50 | arena p50 |
|----------|-------|---------:|----------:|------:|---------:|----------:|
| byslug   | 1-row `@sql json`  | 57071 | 55401 | **−2.9%** | 246µs | 248µs |
| profile  | 1-row typed        | 68475 | 66973 | **−2.2%** | 208µs | 212µs |
| list     | 20-row `@sql json` |  6327 |  6149 | **−2.8%** | 1965µs | 2055µs |
| comments | multi-row `@sql json` | 20387 | 19866 | **−2.6%** | 687µs | 709µs |

## Reading

The arena is **~2.5% slower end-to-end on this workload**, uniformly across
every endpoint. That uniformity is the tell: it is a small *fixed per-request*
cost, not a per-allocation or shape-dependent effect.

The malloc/free bucket the arena removes (measured at ~31.7% of host CPU in an
earlier list-endpoint *host-only* microprofile) does **not** translate into an
end-to-end win here, because these endpoints are SQLite-bound: for the three
`@sql json` shapes the JSON document is assembled inside SQLite and the host
mostly passes a string through, so host-side Roc allocation is a small slice of
total request time — and the bump savings on that slice are tiny in absolute
terms.

What is *not* the cause: the per-`roc_alloc` `Mutex`. These endpoints make only
tens of host allocations per request; an uncontended lock is ~15ns, so the lock
is well under 0.1% of a request measured in hundreds of µs to ms. The 2.5% is
the arena's **fixed per-request scaffolding**, paid whether or not the request
allocates much: minting `Arc<RequestArena>` (a heap alloc + `Mutex` init), the
`catch_unwind` frame, three extra `Arc` clone/drop pairs (guard, body owner,
`current_arena`), the TLS install/uninstall, and one 64 KiB backing-chunk
`malloc`/`free` that replaces the small-alloc churn. On a request whose host
work is dominated by SQLite, that scaffolding is pure tax.

The A3 zero-copy body borrow does not offset it either — the small bodies
(`byslug`, `profile`) fall under the slow-drain copy threshold (so they still
copy, and now also pay the arena bookkeeping to *decide* to copy), and the
large ones (`list`, `comments`) are dominated by SQLite assembly time, not the
body copy.

## Honest conclusion

As implemented the arena is **at parity to slightly negative** (~2.5%) on the
RealWorld read endpoints — inside the ±5% band the turso workstream's
regression gate already tolerates, but *not* the win the 31.7% host-CPU figure
suggested, because those endpoints spend most of their time in SQLite, not in
host allocation.

This bench does not by itself justify the arena on throughput. What it can't
measure here, and what the design was also chosen for:

- **A3 zero-copy bodies** pay off as body size and concurrency grow past a
  single laptop's 16 connections — not visible in these SQLite-bound shapes.
- The arena is the **substrate for B4's zero-copy DB borrows** (a region with a
  request-scoped lifetime is the precondition), independent of this number.
- A more allocation-heavy handler (typed multi-record assembly rather than
  `@sql json` passthrough) is where the bump would earn its keep — not
  exercised by the current RealWorld reads.

Isolating and shrinking the fixed scaffolding (e.g. reusing a per-blocking-
thread arena across requests instead of minting one each time, so `Arc`/`Mutex`
setup amortizes) is the obvious follow-up if the arena is kept. That is a
design change the plan did not name, so it is a decision to raise, not to make
unilaterally.

Leak/soundness is settled independently of throughput: 25 host unit tests
(arena tag/no-op-dealloc/bulk-free, align-16 realloc, foreign-thread teardown,
double-free canary, both guard-drop orderings) plus 154/154 RealWorld
conformance under the live arena.
