# Phase 4 — query canonicalization (fail-safe subset)

**Date:** 2026-08-29. `just check` exit 0, realworld Hurl 154/154, host 57.

## What shipped

Canonical query keying that collapses junk-param spellings of one query onto a
single cache entry — the query analogue of the shipped body-key mechanism
(D14), implemented at the base-target level.

- Roc: `Host.set_query_key! : U64, U64 => {}`; `Req.query_record!` reports
  `Hasher.hash_of(decoded_query_record)` after a clean decode. A route decodes
  its query more than once (bounds + filters), so keys ACCUMULATE and fold.
- Host: `CacheAttribution.query_keys` (Vec, folded like body_keys);
  `CacheDeps.query_key`; `roc_set_query_key`. On store, when a query key was
  reported, the entry is stored under a canonical target
  `{path}?~q{fold:016x}` and the raw target is aliased to it. On lookup, a raw
  miss retries once through the target alias.
- The alias table is a raw-target -> canonical-target map, keyed on the FULL
  raw target (which includes the path, so different routes cannot collide —
  review S1) and bounded with FIFO eviction, never a wholesale clear (review
  S4).

Verified end-to-end: `?limit=5`, `?limit=5&utm_source=spam&fbclid=xyz`, and
`?utm=1&limit=5&ref=2` all serve one entry (same ETag; the repeat logs a
"canonical query-key hit"); `?limit=10` stays a distinct entry.

## What was DEFERRED, and why (the fail-open half)

Drop-when-unread — a route that reads NO query serving one entry regardless of
query — is NOT implemented. It is fail-OPEN (erasure serves a wrong response),
and the review showed it is only sound if EVERY query read is host-attributed,
which requires `query` OFF the request record (a new `Host.request_query!`, the
field removed from the requires-block AND every app's request-type annotation,
e.g. connect's `query : Str`). That ABI ripple plus the static-route-attribute
machinery the drop needs is a separate, larger change.

Canonicalization here is FAIL-SAFE by contrast: a route that never decodes its
query reports no key and stores under its raw target unchanged — no drop, no
cross-route hazard. The cost is that a query-unread route (Tags) still varies on
raw query, so its junk-param DoS is not mitigated (only decoding routes —
list/feed — are). That is the same exposure as before this phase for those
routes; nothing regresses.

Also not addressed (review S8, acknowledged): the canonical key hashes the
PRE-clamp decoded record, so `/articles` (limit absent) and `/articles?limit=20`
(explicit 20) are distinct entries though they render alike — a missed collapse,
never a wrong answer.

## Remaining phases

5 (FieldsEncoding), 6 (sqlgen fixed-column reader), 7 (SQL param encoder /
named binding), 8 (file parser_for! upstream). Query-off-record + drop-when-
unread remain available as a follow-up if the fail-open surface is wanted.
