# Materializing the viewer-independent article aggregates

**Date:** 2026-07-15 · seeded DB (5000 articles) · macOS aarch64 · follows the
request-CPU-profile work (`2026-07-15-request-cpu-profile.md`), which left the
list page's frontier at the SQL itself.

## Starting point: the plans were already optimal

The list page ran four correlated subqueries per row (up to 80 for a 20-row
page). `EXPLAIN QUERY PLAN` showed every one hits a **covering index** and the
page scan is a reverse-PK walk that early-exits at LIMIT — no missing index, no
bad plan, no temp b-tree. Genuine SQLite VDBE compute ≈ 53µs/page; marginal cost
of each subquery:

| subquery | µs | note |
|---|---:|---|
| tag_list | 11.5 | nested GROUP_CONCAT/ORDER BY co-routine — priciest |
| favorites_count | 6.6 | COUNT(*) aggregate |
| favorited (viewer) | ~0.6 | covered EXISTS |
| following (viewer) | ~0 | covered EXISTS |

The only way to run *fewer* subqueries (plans can't improve) is to stop
recomputing what doesn't change per viewer.

## What was measured, and the one correction to the plan

Split the summary into **viewer-independent** (favorites_count, tag_list —
identical for everyone) and **viewer-dependent** (the two flags). Options tested
on a materialized copy (µs/page):

| approach | authed | anon |
|---|---:|---:|
| current (4 subqueries/row) | 53 | 53 |
| materialize expensive cols, **flags fixed up via Roc-side membership queries** | 44 | — |
| materialize expensive cols, **keep flags as SQL subqueries** | 38 | 32 |

The "fix flags up on the Roc side" idea is **counterproductive**: the flags are
already covered-index EXISTS probes (~0.6µs); replacing them with two batch
membership queries adds ~15µs of round-trips + FFI to displace something SQLite
does in one seek. So: materialize the two expensive aggregates, keep the flags
in SQL, and constant-0 them only for the anonymous list (viewer 0).

## Implementation

- `articles` gains `favorites_count INTEGER NOT NULL DEFAULT 0` and `tag_list
  TEXT`, maintained by four AFTER INSERT/DELETE triggers on favorites /
  article_tags. Triggers are `## @schema` (single source of truth; sqlgen
  applies them to its in-memory analysis DB, harmless there). turso 0.7.0
  implements triggers fully (VDBE subprograms) — verified 154/154.
- `article_select` / `article_summary_select` splice `a.favorites_count` /
  `a.tag_list` instead of the two subqueries. Generated types unchanged
  (`I64` / `NullableStr`), so the row mappers didn't move.
- `article_summary_select_anon`: no `:viewer`, `0 AS favorited/following`.
  `run_article_page!` picks it when `viewer == 0` and drops the leading viewer
  param. Feed always passes a real viewer; byslug stays as-is (single row).
- sqlgen fix: `split_statements` tracked no block depth and shredded a trigger's
  `BEGIN ... ; END` on the inner `;`. Now counts BEGIN/END words and only splits
  at depth 0 (test: `keeps_trigger_begin_end_body_as_one_statement`).
- `bench/seed.py` adds the columns and backfills them (read-only bench never
  fires the triggers).

## Result (wrk, 5000-row seed, 2-pass order-balanced)

| endpoint | rusqlite base→mat | turso base→mat |
|---|---|---|
| **list** | 7031 → **7982 (+13.5%)** | 3455 → **3615 (+4.6%)** |
| byslug | 59160 → 62033 (+4.9%) | 57472 → 60918 (+6.0%) |
| comments | parity (untouched) | parity (untouched) |
| tags | parity (untouched) | parity (untouched) |

rusqlite gains **more** than turso, inverting the "turso's DB is 73% so it wins
bigger" prediction: turso's list is bottlenecked in cursor-stepping / VDBE row
iteration, not the subqueries removed here, so eliminating them helps less
proportionally. No regressions; 154/154 on both engines.

## Reproducing

```sh
python3 bench/seed.py bench/seeded.db
just build examples/realworld/main.roc && cp main bench/srv-mat
git checkout <pre-mat> -- examples/realworld && just build … && cp main bench/srv-basemat
# then 2-pass: base,mat then mat,base (see scratchpad qbench.sh)
```
