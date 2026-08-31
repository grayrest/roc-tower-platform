# Phase 1 — vendored JSON encoder (`JsonEnc`)

**Date:** 2026-08-04. Commit `0c80001`. `Res.json` now encodes response bodies
through the platform's own `JsonEnc` format instead of the builtin `Json.to_str`
+ Str→bytes round trip.

## Correctness — settled

- realworld Hurl conformance **154/154**, byte-identical wire output (the
  conformance suite asserts exact bodies, so any drift fails it).
- platform expects 125/125, realworld expects 154/154, `just check` exit 0.
- The escaper is covered directly in `JsonEnc.roc`: quotes, backslash,
  `\n`/`\r`/`\t`/`\b`/`\f`, `\u00XX` for other controls, the no-escape fast
  path, and raw multi-byte UTF-8 pass-through.

## Performance — the end-to-end bench cannot measure this, by construction

`bench/run.sh` drives anonymous GETs against the realworld app, which runs with
`ResponseCache` enabled. Anonymous GETs are cache-HITS after warmup, served from
the stored byte buffer — **the encoder never runs on the measured path.** The
observed rps is therefore uniform across endpoints (~141k on this machine for
byslug/list/comments/tags alike), which is HTTP-stack + cache-lookup throughput,
not encode throughput. It tells us the encoder didn't regress the hit path (it
can't — it isn't on it), and nothing about the encode cost itself.

Measuring the encode path properly needs a cache-MISS workload (authenticated
requests bypass the cache, or `ResponseCache` disabled). That microbench was not
built; the plan's "bench gate on phase 1" is not answerable with the existing
harness, and the honest status is: **encode-path cost unmeasured; hit path
unaffected; output correct.**

## The known cost, unmeasured but real

The builtin appends a `Str`'s bytes into its output buffer with `u8_list_reserve`
+ `str_get_utf8_byte_unsafe` (compiler-only, one allocation-free copy). `JsonEnc`
uses `List.concat(out, s.to_utf8())`, one temporary byte list per string. On a
miss-heavy workload this could plausibly cost more than the Str round trip it
removes for string-dense bodies, or less for number-dense ones. Unknown until
measured against a miss workload. Recorded so the claim isn't overstated.

## Measured 2026-08-29 (miss-path)

Isolated encoder micro-bench (200k iterations, a representative nested
response record with Str/I64 fields, `favoritesCount: i` varied per iter so the
encode can't be hoisted; in-process, no cache, no DB):

    JsonEnc.to_utf8_with_capacity : ~1719 ms  (~8.6 µs/encode)
    Json.to_str(...).to_utf8()    : ~2291 ms  (~11.5 µs/encode)   [builtin]

**JsonEnc is ~25% faster**, byte-identical output (77,888,895 bytes both). The
direct-to-bytes write beats the builtin's Str build + `.to_utf8()` copy even
with the per-string `to_utf8` allocation the vendoring cost note worried about.
The open question in this note is resolved: phase 1 is a win, not a wash.
(Measured on a plain record so the builtin resolves; NullableStr-bearing bodies
use JsonEnc's custom encoder either way.)
