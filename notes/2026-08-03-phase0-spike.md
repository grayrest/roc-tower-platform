# Phase 0 spike — userland Encoding format across the platform boundary

**Date:** 2026-08-03, **corrected 2026-08-04** against a repaired `main` and the
stable toolchain (`~/.bin/roc`, `release-fast-917abf7f`).

**Result: PARTIAL PASS.** The mechanism works across the platform/app boundary.
**The unannotated case — plan decision 2's core bet — does NOT work on this
compiler.**

Artifacts (THROWAWAY — delete before phase 1 lands):
`platform/QuerySpike.roc`, `examples/spike/main.roc`, plus `QuerySpike` in
`platform/main.roc`'s `exposes` list.

## Correction to the first version of this note

The original write-up claimed all four gates passed and that the spike app added
"zero new diagnostics." **That measurement was invalid.** It was taken as a
delta against a red baseline (18 pre-existing errors, see
`notes/2026-08-04-toolchain-catchup.md`), and among those 18 the app's
`import pf.QuerySpike` was failing outright with `PACKAGE MODULE IS PRIVATE` —
the module was never added to the platform's `exposes` list. The app never
compiled the decode at all, so gate 1 was not tested. The 11 in-module expects
were real and did pass; they just don't reach the boundary.

The original note also blamed the 18 errors on the local `write-mask-transform`
roc build. They reproduce identically on the stable build. That was wrong too.

Lesson worth keeping: **do not measure a gate as a delta against a red
baseline.** Repair first, then measure absolutely.

## What actually holds

| Gate | Result |
|---|---|
| 1. A format defined in a PLATFORM module drives a derived parser consumed from an APP | **PASS, with an annotated target.** `roc check` clean; built and ran: `GET /` → `limit=21 tag=roc errors=0` (20+1, tag off the query, `utm=junk` skipped) |
| 2. An ABSENT optional field decodes to `Err(Missing)` without the format knowing it was optional | **PASS** — the driver resolves optionality from the `Try(_, [Missing])` row; the format only ever sees `FieldName.name` |
| 3. A MISTYPED field accumulates, and a SECOND mistype also accumulates | **PASS** — `limit=abc&offset=xyz` yields two `InvalidField`s and a completed parse |
| 4. An absent REQUIRED field reports `MissingRequiredField(name)` | **PASS** |
| 5. (implicit, and the one that matters) The target shape INFERRED from field accesses | **FAIL** |

`roc test --no-cache platform/QuerySpike.roc` → All (11) tests passed.

## The blocker: field accesses do not close the record row

With no annotation, `page.value.limit` and `page.value.tag` leave the row OPEN,
and `parser_for` will not resolve against it. The compiler prints the target as:

    { limit: I64, tag: [Err(_b), Ok(Str)], .. }
                                           ^^ still open

Adding `page_result : Try({ value : { limit : I64, tag : Try(Str, _) }, errors :
_ }, _)` makes it check clean and run correctly. So the ONLY thing missing is
row closing — this is the "a concrete type has been needed to get a decoder to
match" bug, still present in `release-fast-917abf7f`.

Two sub-findings while narrowing it:

- The optional field needs a **wildcard error row**. `Try(Str, _)` pins to the
  generic `[Missing]` optional marker (upstream `ParserWildcardOptionalField
  .roc` documents this). Writing `match … { Err(Missing) => … }` at the use site
  CLOSES the row to `[Missing]`, and the derive then treats the field as an
  ordinary tag union and demands `parse_tag_union` on the format. So even under
  a future row-closing fix, use-site matching has to leave the error row open
  (`Err(_)`) or the annotation has to supply the wildcard.
- Exposing a platform module is a two-line change: the `exposes [...]` list at
  `platform/main.roc:74` AND an `import` near the bottom. Missing the first
  fails with `PACKAGE MODULE IS PRIVATE`, which is easy to lose in noise.

## Consequences for the plan

- **Phases 1-2 are unaffected.** Body decode targets are already annotated
  everywhere in-tree (`Users.Registration`, `Users.UserPatch`,
  `Articles.NewArticle`), so vendoring the JSON format and rewiring `Res.json`
  do not depend on inference at all.
- **Phases 3-4 lose their headline ergonomic claim** until row closing lands
  upstream. They still deliver the query-variant DoS fix, the accumulation, and
  the boundary type conversion — but with an annotated `ListQuery`-style type,
  which is what the adversarial review recommended anyway.
- Decision 2 should be restated as: unannotated is the specification TARGET and
  the mechanism is built for it, but the branch ships annotated and picks up the
  inference win for free when upstream closes rows. Nothing in the format
  implementation changes either way — verified, since the same format handled
  both spellings here.

## Other testing facts confirmed

Platform-module expects only run when the module is reachable from
`platform/main.roc`. `roc test examples/spike/main.roc` runs the PLATFORM's test
set and does not pick up an app-imported module's expects; only
`roc test platform/QuerySpike.roc` runs them. Decision 10's coverage story needs
the Justfile's `test-roc` recipe to name each new platform module explicitly.
(Now that `QuerySpike` is in `exposes` + imported by `main.roc`, its expects DO
join the platform run — which is the mechanism to rely on.)
