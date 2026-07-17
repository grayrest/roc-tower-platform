# Upstream Json ParseErr enrichment — scoping assessment (EX3)

**Date:** 2026-07-17 · deferred, not attempted — the landing sites are deep in
the derive machinery and the platform is already structured to absorb the
change with zero app churn when it lands.

## Goal (from plans/2026-07-17-input-extractors-cache-keys.md)

1. `InvalidField(name, expected)` in `Json.ParseErr` — a present-but-wrong-
   typed field should name the field (and ideally the expectation) instead of
   collapsing into the detail-free `InvalidJson("Invalid JSON")`.
2. Error ACCUMULATION in the record decoder — report every bad/missing field
   in one response, not first-failure.

## Where the changes actually go (roc repo)

- The derived record parser's error paths are EMITTED IR, not Roc source:
  `src/postcheck/monotype/lower.zig` ~23120–23250 builds the per-field parse
  expression. The absent-field path calls the encoding's
  `missing_record_field` hook (3-ary, name in scope); the PRESENT-but-failed
  path just propagates the field parser's error — there is no hook receiving
  the field name there today.
- Adding `InvalidField` therefore needs: a new encoding hook (e.g.
  `invalid_record_field : JsonEncoding, Str, ParseErr, JsonState -> ParseErr`
  in `Builtin.roc`'s Json), a new `missingRecordFieldError`-style emitter in
  lower.zig wrapping the field-failure branch, and the `Json.ParseErr` union
  gaining the variant (grep consumers; platform `Req.explain` already has a
  wildcard).
- Accumulation is bigger: the generated parse loop short-circuits through
  `Try`; collecting per-field errors means threading an accumulator through
  the emitted loop and changing `parse`'s error contract (`ParseErr` vs
  `List(ParseErr)`), i.e. a new entry point or a breaking change.

## Why the platform doesn't block on it

`Req.json_body!` already rejects `InvalidBody(List(Json.ParseErr))` — a
singleton list today. When upstream lands either half, the payload gets
richer and `Req.explain` gains a branch; no platform union or app change.
