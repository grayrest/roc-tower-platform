# Where the CPU goes in a request (byslug + list)

**Date:** 2026-07-15 · seeded DB (5000 articles) · macOS aarch64 · two lenses:
an env-gated in-host bucket timer (`TP_PROFILE=1`, single-connection so wall≈CPU)
and Apple's `sample` (self-time attribution across the whole stack under load).

## Service-time split (`TP_PROFILE`, single-connection)

Server-side time (excludes HTTP parse), averaged over ≥20k requests:

| bucket | byslug rusqlite | byslug turso | list rusqlite | list turso |
|--------|----------------:|-------------:|--------------:|-----------:|
| host framework (route, arena, ctx, body) | 10.4µs (32%) | 12.0µs (28%) | 32µs (6%) | 36µs (2%) |
| **DB execute** | 7.8µs (24%) | 16.1µs (37%) | 168µs (31%) | **1055µs (73%)** |
| **Roc-side** (build records + `Json.to_str`) | 14.8µs (45%) | 15.5µs (36%) | 340µs (63%) | 360µs (25%) |
|  ↳ cell marshalling (of Roc-side) | ~1.1µs | ~1.1µs | ~31µs | ~31µs |
| **total (dispatch)** | 33µs | 43.6µs | 540µs | 1450µs |

Reading:
- The **Roc-side response construction dominates on rusqlite** — 45% (byslug),
  63% (list) — and it's engine-independent (turso's Roc-side ≈ rusqlite's).
- Per-article Roc-side is ~15µs (byslug, positional read) vs ~17µs/article
  (list, by-name read) → the **by-name `Sql.query!` penalty is only ~2µs/article**;
  the shared ~15µs is record-building + serialization.
- **Marshalling (the zero-copy target) is 3–6%** — confirming it's not the lever.
- **Turso's gap is DB execution**, query-shape dependent: 2× on byslug (diluted
  to parity under concurrency), **6.3× on the list's correlated-subquery page**.

## What the Roc-side 340µs actually *is* (`sample`, list, rusqlite)

On-CPU self-time (idle threads excluded), by category:

| bucket | share | note |
|--------|------:|------|
| memory allocation | ~25% | arena `try_alloc` 11.7% + **`madvise` freeing the chunk 10.9%** + `RocList.reallocate` + `roc_alloc` |
| memcpy (`_platform_memmove`) | ~20% | cell→RocStr, list growth, record moves, serialize output |
| SQLite (`VdbeExec` + btree) | ~26% | the two queries |
| Roc logic (`roc__proc_*`) | ~11% | handler + mappers + `Json.to_str` control flow |
| str/list/utf8 builtins | ~6% | ~5% is *lossy UTF-8 re-validation* on already-valid DB text |
| refcount + mutex/TLS + misc | ~12% | |

**~half the CPU is memory: allocation + copying + refcounting** — the churn of
materializing 20 nested typed records and walking them to emit JSON. The actual
Roc *compute* is only ~11%. SQLite is ~a quarter.

## Levers this exposed

1. **Arena chunk alloc/`madvise` was ~22% of list CPU** — the mint-per-request
   64 KiB chunk was `madvise`-freed every request (`Arc::drop_slow → free_medium
   → mvm_madvise_free_plat`). **Fixed:** a warm-chunk freelist (arena.rs) recycles
   standard chunks. Confirmed by re-sampling: chunk churn **22.7% → 3.8%**, and
   **list throughput +30%** (byslug +1.7%, comments flat, 154/154). This reverses
   the earlier "amortization = no gain" verdict, which was measured on the
   DB-bound `@sql json` workload. Now **default-on**.
2. **~5% lossy UTF-8 re-validation** on DB text that is already valid UTF-8 —
   the RocStr build / Json encode re-scans it. Skippable. (Not yet done.)
3. **`RocList.reallocate` from the fold-append** building the 20-item list —
   `List.with_capacity` would cut regrow/copy. (Not yet done.)

## Reproducing

```sh
# service-time split
TP_PROFILE=1 RUST_LOG=info PORT=8199 ./main   # then: wrk -t1 -c1 -d10s <url>; read the [profile] log line
# self-time attribution
PORT=8199 ./main & sample $! 10 -file out.txt   # under wrk -t4 -c16 load
```
