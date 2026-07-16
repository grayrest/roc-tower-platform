# Derived response cache: build results (DC1–DC5)

Implementation of `plans/2026-07-16-authorizer-derived-cache.md` (decision
log: `notes/2026-07-16-derived-cache-decision-log.md`). Five phases, one
commit each. **Caching is now automatic**: no `Res.cacheable`, no
hand-declared dep lists, no sqlgen `_tables`, no annotation of any kind —
an app that enables the `ResponseCache` setting gets sound, precisely
invalidated caching for every store-eligible response.

## What shipped

- **DC1 — analysis shadow** (`crates/host/src/shadow.rs`): per-database
  in-memory rusqlite mirrors (`foreign_keys=ON`); `analyze()` prepares
  against the shadow with the authorizer installed and interprets the
  callbacks into `{reads, writes, nondeterministic, schema_change, opaque}`,
  memoized per SQL text (capped). The two load-bearing assumptions are
  test-proven in-repo: preparing a DML fires trigger sub-program callbacks
  (the cascade expansion — no trigger-body parsing anywhere), and pragma-ON
  FK cascade deletes report the child-table write.
- **DC2 — attribution + coeffects**: `sql_begin` records each request-time
  statement's derived read set onto the ctx (fail-closed poison; read-path
  DML purges AND poisons); ctx-0 DDL mirrors into the shadow. Attribution is
  taken on the handler thread and rides the response as an extension.
  Nondeterministic host primitives poison via an explicit install/clear
  thread-local; only **Must** DbWrite/HttpSend effects poison (D12 — Log and
  all Opt effects are instrumentation-neutral). The request record lost
  `body` and the `Cookie` header: both are host-mediated coeffects
  (`Req.body!`/`json_body!`/…, `Req.cookie(name)`) that self-report, which
  is what makes their cache variance platform-known (D13/D14).
  `Sql.migrate!` pins ctx 0 and lost its ctx arg (R12).
- **DC3 — the store**: base key `(method, target, encoding)` → variant sets
  (spec = cookie names read + body extracted + `Vary` names); column-scoped
  `(table, Cols)` deps with intersection purging; per-table generation
  counters sampled before each first read and re-checked at insert close the
  read-then-store race (R13); `purge_all` bumps an epoch with the same
  handshake.
- **DC4 — wire-through**: `maybe_store` consumes the extension under RFC
  9111 shared-cache rules (2xx, derived-cacheable, no
  `Cache-Control: no-store`/`private`, no `Vary: *`); one shared
  `variant_hash` computes ids on both store and lookup; body-varying specs
  buffer + hash the request body (the service chain now carries a boxed
  request body end to end). `effect.rs` purges by shadow-derived,
  trigger-expanded, column-scoped write sets; runtime DDL flushes everything
  after mirroring; underivable writes fall back to the legacy parse, else a
  full flush (never silent staleness).
- **DC5 — surface deletion**: `Res.cacheable`, `x-tp-cache-tables`, the five
  `Db.*_read_tables` lists, and sqlgen's `_tables` emission are gone.
  `bench/cache-check.sh` asserts the derived semantics (including comments
  invalidation and the documented follow-purge behavior, D11/R14).

## Store-eligibility gating (the perf-critical bit)

The middleware marks store-eligible requests with a `CacheEligible` request
extension; the router threads it into the handler thread's tracking flag.
**Bypassed traffic (authorization header, non-configured method, cache
disabled) skips attribution entirely — zero extra lock acquisitions.**
Without this, authed profile traffic measured −20%; with it, +3%.

## Benchmarks (rusqlite, release, M-series laptop, wrk -t4 -c16 -d15s)

Baseline = pre-derived-cache binary (`fa9b0d1`), same seeded DB.

| path | endpoint | baseline | derived | Δ |
|---|---|---|---|---|
| hit (anon) | byslug | 143.9k | 140.1k | −2.6% (run-to-run scatter ±2%) |
| hit (anon) | profile | 145.3k | 141.5k | −2.6% |
| hit (anon) | list | 139.8k | 141.0k | +0.9% |
| hit (anon) | comments | 141.5k | 142.7k | +0.8% |
| hit (anon) | tags | 140.9k | 141.8k | +0.6% |
| bypass (authed) | profile | 58.1k | 59.9k | +3.1% |
| bypass (authed) | list | 7.4k | 8.3k | +11% (Roc-bound, noisy) |
| store-miss (unique URLs) | profile | 10.9k | 40.1k | **+267%** |
| store-miss (unique URLs) | list | 5.4k | 8.4k | **+55%** |

The store-miss win is an accidental fix: the old cache's eviction ran an
O(n) `order.retain()` per removal — quadratic under churn at the capacity
cap. The new store leaves dead FIFO pairs to be skipped lazily.

R16 gate (miss ≥ baseline − 3%, hit within noise): **passes everywhere**.

## Verification

- 51 host tests (11 shadow, 11 store), 12+11 sqlgen, 23+2 e2e, 86+115 roc
  expects, clippy clean.
- Conformance 154/154 on **rusqlite AND turso** — the rusqlite shadow derives
  for the turso data engine transparently; `bench/cache-check.sh` passes on
  both binaries.
- Live probes beyond the suites: automatic storage proven by a stale serve
  after an out-of-band sqlite write (no purge signal exists — the entry was
  really served from memory), and derived purge proven by a real `DbWrite`
  refreshing the same entry (trigger-expanded write set).

## Accepted costs / known behavior (decision log D11/R14)

Derivation honestly re-adds `follows` to by-slug, profile, and comments
deps (their SQL carries a `following` EXISTS): every follow write purges
those entries. Anonymous correctness is unaffected (the recomputed body is
identical); the cost is hit-rate under follow-heavy write mixes. Recovery is
app-side anon query variants (as the list already has — its derived deps
are exactly the old hand-tuned set). List/tags derive identically to the
deleted hand lists.

Nondeterministic SQL functions poison via the authorizer's `Function`
callback; bare `CURRENT_TIMESTAMP` is a keyword, fires no callback, and is
the documented exclusion.

## Follow-ups (out of scope, tracked in the decision log)

Read-after-write handler design (D6 — hello/e2e still use mid-request
`migrate!` for their demo counters); anon query variants for
by-slug/profile/comments (D11); authed caching (D5); compiler touched-set
API (D2).
