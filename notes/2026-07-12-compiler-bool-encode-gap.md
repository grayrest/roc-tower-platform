# Compiler gap: derived JSON encoder stringifies Bool

**Found**: 2026-07-12, building the RealWorld example.
**Status**: worked around in the platform (`platform/JsonBool.roc`); queue a
compiler session to fix the derivation, then delete the wrapper.

## Symptom

```roc
Json.to_str({ favorited: True, following: False })
# actual:   {"favorited":"True","following":"False"}
# expected: {"favorited":true,"following":false}
```

The derived record encoder treats `Bool` like any user tag union and emits
the tag NAME as a JSON string.

## Why it's clearly a bug, not a choice

- `Json.encode_bool : JsonEncoding, Bool, JsonEncodeState -> ...` already
  exists in Builtin.roc (~line 90) and works when called directly (the
  `JsonBool` wrapper routes through it).
- The DECODE direction is already special-cased: `Json.parse("{\"ok\":true}")`
  into `{ ok : Bool }` succeeds via `parse_bool`. Encode/decode are
  asymmetric.

## Reproducer

```roc
module []

expect Json.to_str({ ok: True }) == "{\"ok\":true}" # fails: got "{\"ok\":\"True\"}"

expect {
    parsed : Try({ ok : Bool }, Json.ParseErr)
    parsed = Json.parse("{\"ok\":true}")
    parsed == Ok({ ok: True }) # passes — decode already special-cases Bool
}
```

## Where to look

Encoder derivation in `src/check/Check.zig` / `src/postcheck/monotype/lower.zig`
(the `encoder_for` derivation path). Decode presumably special-cases the
`Bool` nominal before falling back to tag-union derivation; encode needs the
same check, dispatching to the format's `encode_bool`.

## Platform cleanup once fixed

- Delete `platform/JsonBool.roc`; drop it from `main.roc` exposes/imports.
- `examples/realworld`: switch `Db.Profile.following`, `Db.Article.favorited`
  flag fields back to plain `Bool` and remove `JsonBool.from`/`truth` calls.
