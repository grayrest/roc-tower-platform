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

## Phase 6 measured 2026-08-29 (miss-path)

Authenticated list request (`GET /api/articles?limit=20` with a token —
`authorization` is hard-defaulted into the cache bypass set, so every request
runs the full handler: auth lookup + dynamic page query + 20×12-column row read
+ encode). wrk -t4 -c16 -d15s, two runs each:

    by-name (Sql.Row)          : ~10201 rps  p50 ~1.17 ms
    positional (@reader)       : ~10810 rps  p50 ~1.13 ms

**Positional is ~6% faster** end-to-end. The row read is only a fraction of the
request (auth + DB + encode dominate), so the isolated read-path win is larger
but diluted; positional avoids 240 by-name column lookups (find_first_index over
the column-name list) per 20-row page.

## Uncached list end-to-end, count fix confirmed (2026-08-30, load 4.5)

TP_PROFILE single-conn, anonymous list, cache off, WITH the count-join fix
(commit 20cdf8c):

    req = 99,811ns = host 13,118ns + handler 86,693ns
                     [ db_exec 13,112ns + roc_side 73,581ns (~18µs marshal) ]

vs before the fix (231µs; db_exec 147µs). The count fix removed ~134µs of
db_exec, cutting the request 57% (231 -> 100µs) and lifting single-conn
throughput 2,667 -> 7,609 rps. The profile FLIPS: the uncached list is now
~74% roc_side (record build + JsonEnc encode), 13% db_exec, 13% host — DB-bound
only until the pointless count join was removed. This is where phases 1/6 act,
so they now move the uncached path.
