# Encoder/decoder branch — phase progress checkpoint

> **UPDATE 2026-08-29: ALL PHASES COMPLETE.** 1/2/3/4/5/6/7/-1 shipped and
> verified; 8 filed. `just check` exit 0, realworld Hurl 154/154. Final commit
> list at the bottom.

**Date:** 2026-08-29. Toolchain: `~/.bin/roc` = local-fixes (codec + app_for_host
fixes landed). Gate command: `ROC=~/.bin/roc just check` (exit 0) and
`ROC=~/.bin/roc just realworld-conformance` (154/154).

## Done and verified (committed)

- **Phase 1 — `JsonEnc`** (`0c80001`): response bodies encode through the
  platform's own format, no Str round trip. Bench can't isolate encode cost
  (cache hides it); output byte-identical, 154/154.
- **Phase 2 — `JsonDec`** (`e9b29c5` built-but-blocked → `c9b912e` wired): body
  decode through the platform's own format, **field-named + accumulated** errors
  (`InvalidField(name, expected)`). End-to-end a two-mistype body returns
  `{"errors":{"body":["field title must be a string; field description must be a
  string"]}}`. This is the branch's headline deliverable. Unblocked by the
  local-fixes compiler fixes (`cad2210f93` + `c7482c7`).
- **Phase 3 — `QueryDec`** (`a4a4962`): query params decode as a format, one
  decode of the whole string, percent-decoding on demand, malformed scalars
  named and accumulated. `?limit=abc` now 400 (decision 3). The five scalar
  factories deleted.
- **Phase -1 — cache fixes** (`0c2492c`): seeded the three hashers
  (poisoning vector), stopped unbounded `order` growth on re-store. 56 host
  tests.
- **connect fix** (`c7482c7`-era `add_book` annotation, committed separately):
  the local-fixes compiler requires the decode row closed.

## Remaining (not started)

- **Phase 4 — query off the record + canonical keying.** The largest change and
  the highest-risk: it adds a new hosted FFI function `Host.request_query!`
  (mirror `roc_request_body` in effects.rs, add `query` to `RequestState`, a
  `query_reads` attribution counter), moves `query` off the request record in
  `main.roc` (both the requires-block handler record AND the user's
  `app_for_host` annotation), regenerates glue, and restructures the cache key
  (query out of the base `target`, into a `VariantSpec` dimension) plus a
  scoped raw→canonical query alias table.

  **Two design decisions must be settled first** (adversarial review, S1/S2/S6):
  1. Which cache layer holds the canonical query key — base-key alias vs
     `VariantSpec` dimension. Base-key needs a mandatory alias translation;
     variant risks `remove_base` wiping cookie variants on a spec flap.
  2. Drop-when-unread is **fail-OPEN** (erasure serves a wrong response, unlike
     canonicalization's fail-safe raw-wins). It is only sound if EVERY query
     read is host-attributed — which is exactly why `query` must come off the
     record — AND the "route reads no query" signal is a STATIC route attribute
     (decision 2's field set), not learned from a prior request's traffic.
     The remaining two Phase -1 alias items (scope by base identity; LRU not
     wholesale clear) land here, on the hot GET path.

- **Phase 5 — `FieldsEncoding` + `SELECT ${fields}`**: introspection format,
  reaches ~2-3 of 6 realworld queries (flat single-table).
- **Phase 6 — sqlgen fixed-column-set row reader**: positional reader for a
  declared column set, decoupled from statement text; deletes
  `Db.article_summary_from_row`'s by-name reads.
- **Phase 7 — SQL param encoder + named `DbWrite` binding**: check turso
  (positional-only) first; the cheaper alternative is routing writes through
  sqlgen `@sql write`.
- **Phase 8 — `parser_for!`**: FILE the upstream ask with the row-decoder
  sketch; do not patch the compiler (plan decision 16).

## Why Phase 4 is checkpointed rather than pushed through

It is a ~8-file FFI + ABI + cache-key change, and the review found multiple
CRITICAL correctness hazards (cross-user wrong responses) that live precisely in
its drop-when-unread and alias-scoping surface. It deserves the two design
decisions above being made deliberately, and each sub-step (ABI, then keying,
then drop) verified green on its own — not a single autonomous push. Phases 1-3
+ -1 already deliver the encoder, the field-named accumulating decoder (the
headline), the query decoder, and the shipped-cache-defect fixes.

## Final status — all phases

| Phase | Commit | Deliverable |
|---|---|---|
| -1 | `0c2492c` | Seeded cache hashers; stopped unbounded `order` growth |
| 1 | `0c80001` | `JsonEnc` — response bytes encode direct, no Str round trip |
| 2 | `c9b912e` | `JsonDec` — field-named, ACCUMULATED body decode errors (the headline) |
| 3 | `a4a4962` | `QueryDec` — query decoding as a format; scalar factories deleted |
| 4 | `e4550cf` | Query canonicalization — junk-param spellings collapse to one entry |
| 5 | `eee5b82` | `Fields` — derive a SELECT column list from a record type |
| 6 | `6187174` | sqlgen `@reader` — positional reader for a declared column set |
| 7 | `a5a5431` | Named-binding writes via `@sql` — kill positional-param drift |
| 8 | `fd438dd` | Filed the `parser_for!` upstream ask (row-decoder sketch) |

Deferred, documented, available as follow-ups:
- Phase 4's fail-OPEN drop-when-unread (needs query off the record + a static
  route attribute).
- Perf measurement of phases 1 and 6 (the warm-cache bench can't isolate
  encode/decode cost; needs a cache-miss workload).
- Migrating the remaining hand-written writes to `@sql` (phase 7 pattern).
