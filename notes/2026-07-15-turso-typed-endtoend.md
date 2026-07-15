# Typed API on Turso — end-to-end (B1+B2)

**Date:** 2026-07-15 · **Build:** `just build-turso` (`--features turso`) vs the
default rusqlite host · seeded DB (`bench/seed.py`) · macOS aarch64.

The whole RealWorld app runs on Turso: `just realworld-conformance-turso` passes
**154/154** — typed cursor reads, `@sql json` reads, write transactions,
rollback, and UNIQUE/PK constraint 422s all correct. This note is the
throughput artifact for the API-direction discussion.

## End-to-end throughput (wrk, order-pooled median of 4 passes)

| endpoint | shape | rusqlite rps | turso rps | slowdown | rusqlite p50 | turso p50 |
|----------|-------|-------------:|----------:|---------:|-------------:|----------:|
| byslug   | 1-row `@sql json`   | 58790 | 14428 | **4.07×** | 242µs | 950µs |
| profile  | 1-row **typed**     | 70499 | 25878 | **2.72×** | 204µs | 584µs |
| list     | 20-row `@sql json`  |  6095 |  2935 | **2.08×** | 2065µs | 3930µs |
| comments | multi-row json      | 19349 | 11079 | **1.75×** | 694µs | 1350µs |

## Reading

The B0 microbench measured Turso at **8–17× slower than rusqlite at the SQL
level** (8× on a trivial point lookup, ~17× on the hot `@sql json` query).
End-to-end that compresses to **1.75–4×**, because a real request spends much of
its time outside SQLite — HTTP parse/serialize, Roc marshalling, the arena — and
that fixed overhead dilutes the engine gap:

- **`byslug`** (single `@sql json` query, most DB-dominated) shows the widest
  gap, 4.07× — closest to the raw engine ratio.
- **`profile`** — the **typed** endpoint, the one that actually exercises the
  cursor-getter + Roc-record path this experiment is about — is **2.72×**. That
  is the headline number for "the typed API on Turso": correct, and ~2.7× the
  cost end-to-end.
- **`comments`** (multi-row, most work per request outside the DB) is only
  1.75× — here the HTTP/marshalling fixed cost is the largest share, so the
  engine matters least.

## For the Roc-devs discussion

Turso being a Rust-native engine was the original draw: its Rust-owned pager
opens the door to zero-copy borrows of column bytes into Roc values (the shelved
B4), which could shrink the typed path's per-cell marshalling — the ~30%-of-CPU
response-construction cost the Phase 7 profile identified. This build is the
substrate for exploring that: the typed API runs on Turso today at a known 2.7×,
and any zero-copy-borrow work would be measured against that baseline. The
counterweight is Turso's 8–17× raw-engine deficit — a borrow optimization has to
claw back a lot before the typed-on-Turso path beats typed-on-rusqlite.

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
