# Phase 2 — vendored JSON decoder (`JsonDec`) — BUILT, BLOCKED

**Date:** 2026-08-04. Status: the decoder is written and its mechanism is
PROVEN at the unit level, but it cannot be integrated into the platform, and —
as of this session — nothing can be validated further because the machine's
`roc` toolchain was swapped mid-phase. Details below.

## What was built

`platform/JsonDec.roc` — a userland Encoding format, the decode companion to
`JsonEnc`. It drives the compiler's derived record/list parser over a `Str`
cursor, names the offending field on a type mismatch (`InvalidField(name,
expected)`), and ACCUMULATES mismatches (fabricate-and-continue, the plan's
decision-7 mechanism) so several bad fields surface in one response. Covers the
shapes the platform's bodies decode: strings, records (nested, optional
`Try(_, [Missing])`), `null`, `List(Str)`, and I64/U64 numbers, with full JSON
string-escape decoding (`\n \t \r \b \f \" \\ \/ \uXXXX`).

Under the toolchain in use at the START of this session (`~/.bin/roc`, the
Aug-1 `release-fast-917abf7f` build), **`roc test platform/JsonDec.roc` passed
14/14**, including: two mistyped fields both reported; absent optional →
`Err(Missing)`; absent required → `MissingRequiredField`; escapes; nested
envelopes; string lists. The mechanism works.

## Blocker 1 — a compiler SIGSEGV integrating it with `NullableStr`

`Req.json_body!` was rewired onto `JsonDec`, and the three nominals'
`parser_for` retyped. Native `roc build` (not `check`, not interpreted `test`)
then SIGSEGVs in custom-codec lowering:

    prepareCustomCodecCall / graphParserResultNodes / graphTagByName
    (src/postcheck/monotype/lower.zig ~39955 / ~1128)

Minimized in `examples/repro/CustomFormatDrivesCustomNominalParser.roc`: a
custom FORMAT (`JsonDec`) driving a derived parser whose shape contains a custom
NOMINAL parser (`NullableStr`) inside an optional field. The builtin `Json.parse`
driving the same `NullableStr` builds fine (that is how realworld shipped through
phase 1); `JsonDec` over plain records/strings/numbers/lists builds fine
(verified by bisection). Only the COMBINATION crashes. This alone would confine
`JsonDec` to non-`NullableStr` bodies (connect, and realworld's Registration/
Credentials/NewArticle/ArticlePatch/Comment), leaving UserPatch on the builtin.

## Blocker 2 — the toolchain was replaced mid-session (overrides everything)

Partway through phase 2, `~/.bin/roc` changed from the **175 MB Aug-1
`release-fast-917abf7f`** build (which compiled realworld and passed Hurl
154/154 in phases 0 and 1 THIS session) to a **281 MB Aug-27 build byte-
identical to `~/Repositories/roc/zig-out/bin/roc`** on branch `local-fixes`
(`5543f6e021`). The new compiler SIGSEGVs building realworld **even at the
committed phase-1 HEAD** — code it built successfully earlier the same day. So
the current toolchain has a codec-lowering regression independent of any phase-2
change, and no phase can be validated against it: `roc build` of the conformance
app crashes before a request is served.

Because of blocker 2, blocker 1 could not be re-confirmed as a distinct bug —
under the swapped compiler, even the builtin path crashes, so "JsonDec's fault"
vs "toolchain regression" cannot be separated right now.

## State of the tree

- `platform/JsonDec.roc` is committed but NOT imported by `platform/main.roc`,
  so it is inert in the platform build (an un-imported module is not compiled by
  `roc build`). It is the intended home for the decoder once the compiler
  supports it; its expects run with `roc test platform/JsonDec.roc`.
- The platform build path (`Req.json_body!`, the nominals, `Res.json`) is exactly
  phase-1 HEAD — green under `917abf7f`.
- `examples/repro/CustomFormatDrivesCustomNominalParser.roc` documents blocker 1.

## Recommendation

1. Restore a `917abf7f`-equivalent `roc` (or whichever build last compiled
   realworld) before attempting phases 3-8 — every remaining phase needs a
   working native build and the conformance gate.
2. With a working compiler, decide blocker 1: file the custom-format × custom-
   nominal-parser SIGSEGV upstream; until fixed, wire `Req.json_body!` to
   `JsonDec` for non-`NullableStr` bodies and keep a builtin path for UserPatch.
3. Do not carry a compiler patch in this branch for either issue (plan
   decision 16 posture).

---

## UPDATE 2026-08-29 — UNBLOCKED and SHIPPED

Both blockers cleared. The `local-fixes` toolchain landed the compiler fixes
(`ee6aef3d0d` codec-target instantiation, `cad2210f93` identical-duplicate
contract calls — the latter was blocker 1) plus the platform `app_for_host`
annotation (`c7482c7`) that had been mis-manifesting as the glue failure and the
ReleaseFast SIGSEGV. Under it, `JsonDec` × `NullableStr` builds cleanly.

`JsonDec` is now WIRED IN:
- `Req.json` / `Req.json_body!` decode through it; `json_body!` rejects with the
  ACCUMULATED `InvalidBody(errors)` when a decode has mismatches.
- `NullableStr.parser_for` retyped onto `JsonDec` (encode already on `JsonEnc`).
- `Req.explain` gained an `InvalidField(name, expected)` branch; messages are
  QUOTE-FREE so they survive `Api.field_error`'s hand-built JSON.
- realworld's body coeffects propagate `InvalidBody` (bare `?`) instead of
  swallowing it, so `render → Req.explain` surfaces the detail.

Verified: `just check` exit 0, platform 128/128, realworld Hurl 154/154, and
end-to-end a body with two mistyped fields returns
`{"errors":{"body":["field title must be a string; field description must be a
string"]}}` — field-named AND accumulated, the phase-2 deliverable. Missing
required fields report `missing required field: <name>`.

The blocker-1 repro (`examples/repro/RecordFieldNominalParser.roc`, the user's
minimization) now builds; it documents a fixed bug.
