# Typed API on Turso — end-to-end (B1+B2)

**Date:** 2026-07-15 · **Build:** `just build-turso` (`--features turso`) vs the
default rusqlite host · seeded DB (`bench/seed.py`) · macOS aarch64.

The whole RealWorld app runs on Turso: `just realworld-conformance-turso` passes
**154/154** — typed cursor reads, `@sql json` reads, write transactions,
rollback, and UNIQUE/PK constraint 422s all correct. This note is the
throughput artifact for the API-direction discussion.

## End-to-end throughput (wrk, order-pooled median of 4 passes)

**All four benchmarked GET endpoints run on `@sql json`** (Phase 7c migrated them
all), which the B0 note now identifies as turso's *one* weak spot — so these
numbers reflect turso's worst case, not the typed path.

| endpoint | shape | rusqlite rps | turso rps | slowdown | rusqlite p50 | turso p50 |
|----------|-------|-------------:|----------:|---------:|-------------:|----------:|
| byslug   | 1-row `@sql json`   | 58790 | 14428 | **4.07×** | 242µs | 950µs |
| profile  | 1-row `@sql json`   | 70499 | 25878 | **2.72×** | 204µs | 584µs |
| list     | 20-row `@sql json`  |  6095 |  2935 | **2.08×** | 2065µs | 3930µs |
| comments | multi-row `@sql json` | 19349 | 11079 | **1.75×** | 694µs | 1350µs |

## Reading

The B0 microbench (release; see the correction note there) measured Turso at
**~4.5–5.6× slower than rusqlite at the SQL level** (~4.5× on a trivial point
lookup, ~5.6× on the hot `@sql json` query, on the tiny spike seed where
per-query fixed overhead dominates). End-to-end that compresses to **1.75–4×**,
because a real request spends much of its time outside SQLite — HTTP
parse/serialize, Roc marshalling, the arena — and that fixed overhead dilutes
the engine gap:

- **`byslug`** (single `@sql json` query, most DB-dominated) shows the widest
  gap, 4.07× — closest to the raw engine ratio.
- **`profile`** (also `@sql json`, despite an earlier mislabel here) is 2.72×.
- **`comments`** (multi-row, most work per request outside the DB) is only
  1.75× — here the HTTP/marshalling fixed cost is the largest share, so the
  engine matters least.

**Caveat that changes the headline:** these are *all* `@sql json` endpoints, and
the SQL-level measurement (B0 note) shows turso is **at parity with rusqlite on
simple typed reads** and only ~3.75× on `@sql json`. So the end-to-end gap here
is turso's json weakness, not the typed cursor path. A **pure-typed** version of
these endpoints on turso would land far closer to parity — the typed API on
turso is the *good* case, not the 2.72× one. This is the strongest argument yet
for the "eliminate the `@sql json` variant" direction: it removes turso's only
weak spot and would make typed-on-turso ≈ typed-on-rusqlite.

## For the Roc-devs discussion

Turso being a Rust-native engine was the original draw: its Rust-owned pager
opens the door to zero-copy borrows of column bytes into Roc values (the shelved
B4), which could shrink the typed path's per-cell marshalling — the ~30%-of-CPU
response-construction cost the Phase 7 profile identified. This build is the
substrate for exploring that: the typed API runs on Turso today at a known 2.7×,
and any zero-copy-borrow work would be measured against that baseline. The
counterweight is Turso's ~4.5–5.6× raw-engine deficit (release) — a borrow
optimization has to claw back a lot before the typed-on-Turso path beats
typed-on-rusqlite, and B4 found the copy it targets is noise anyway.

## Reproducing

```sh
just turso-sysroot                        # one-time, macOS SDK symlinks
just realworld-conformance-turso          # 154/154 on turso
just build-turso examples/realworld/main.roc && cp main bench/srv-turso
just build      examples/realworld/main.roc && cp main bench/srv-rusqlite
bench/run.sh rusqlite:bench/srv-rusqlite turso:bench/srv-turso
```

The turso backend lives in `crates/host/src/turso_db.rs` (mounted as
`crate::cursor` under `--features turso`); rusqlite remains the default.
