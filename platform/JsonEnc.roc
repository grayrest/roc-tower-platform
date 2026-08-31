## The platform's own JSON ENCODING — a userland `Encoding` format, vendored
## from the builtin's `JsonEncoding` because that type is not reachable from
## user code (upstream hides it on purpose;
## `test/cli/ParserHiddenEncodingTypesRejected.roc` asserts `roc check` fails
## on `Encoding.JsonState`).
##
## Why own it: `Json.to_str` returns a `Str`, so `Res.json` pays a Str→bytes
## round trip on every response. Owning the format means owning the state, so
## the encoder's `List(U8)` IS the response body.
##
## KNOWN COST, measured not assumed (see notes/2026-08-04-phase1-encode.md):
## the builtin appends a `Str`'s bytes straight into its output buffer with
## `u8_list_reserve` + `str_get_utf8_byte_unsafe`, both compiler-only. The
## public equivalent is `List.concat(out, text.to_utf8())`, which allocates one
## temporary byte list per string written. Whether that costs more than the
## Str round trip it removes is an empirical question, and the bench answers it.
##
## Scope: the methods the platform's own shapes actually reach. The protocol
## resolves per encoding, so a shape needing a method this format lacks is a
## COMPILE error naming the method — extend here when that happens rather than
## pre-writing the whole surface.
JsonEnc := [Default].{
    ## Record field names go to the wire unchanged.
    rename_field : JsonEnc, Str -> Str
    rename_field = |_, name| name

    ## A JSON string, quoted and escaped.
    encode_str : Str, JsonEncState -> Try(JsonEncState, [])
    encode_str = |value, state| Ok(JsonEncState.{ output: quoted(state.output, value) })

    ## JSON `true` / `false`.
    encode_bool : Bool, JsonEncState -> Try(JsonEncState, [])
    encode_bool = |value, state| {
        text = if value "true" else "false"
        Ok(JsonEncState.{ output: raw_bytes(state.output, text) })
    }

    ## JSON `null`.
    encode_null : JsonEncState -> Try(JsonEncState, [])
    encode_null = |state| Ok(JsonEncState.{ output: raw_bytes(state.output, "null") })

    encode_u8 : U8, JsonEncState -> Try(JsonEncState, [])
    encode_u8 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, U8.to_str(value)) })

    encode_i8 : I8, JsonEncState -> Try(JsonEncState, [])
    encode_i8 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, I8.to_str(value)) })

    encode_u16 : U16, JsonEncState -> Try(JsonEncState, [])
    encode_u16 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, U16.to_str(value)) })

    encode_i16 : I16, JsonEncState -> Try(JsonEncState, [])
    encode_i16 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, I16.to_str(value)) })

    encode_u32 : U32, JsonEncState -> Try(JsonEncState, [])
    encode_u32 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, U32.to_str(value)) })

    encode_i32 : I32, JsonEncState -> Try(JsonEncState, [])
    encode_i32 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, I32.to_str(value)) })

    encode_u64 : U64, JsonEncState -> Try(JsonEncState, [])
    encode_u64 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, U64.to_str(value)) })

    encode_i64 : I64, JsonEncState -> Try(JsonEncState, [])
    encode_i64 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, I64.to_str(value)) })

    encode_u128 : U128, JsonEncState -> Try(JsonEncState, [])
    encode_u128 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, U128.to_str(value)) })

    encode_i128 : I128, JsonEncState -> Try(JsonEncState, [])
    encode_i128 = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, I128.to_str(value)) })

    encode_dec : Dec, JsonEncState -> Try(JsonEncState, [])
    encode_dec = |value, state| Ok(JsonEncState.{ output: raw_bytes(state.output, Dec.to_str(value)) })

    ## `{ "a": 1, "b": 2 }`. The driver hands us a writer that calls back per
    ## field; comma placement rides in the container state.
    encode_record : JsonEncState,
    U64,
    (JsonEncBox, (JsonEncBox, Str, (JsonEncState -> Try(JsonEncState, err)) -> Try(JsonEncBox, err)) -> Try(JsonEncBox, err)) -> Try(JsonEncState, err)
    encode_record = |state, _count, write_fields| {
        opened = JsonEncBox.{ output: state.output.append(123), needs_comma: False }
        finished = write_fields(opened, JsonEnc.write_field)?
        Ok(JsonEncState.{ output: finished.output.append(125) })
    }

    ## `[1, 2, 3]`.
    encode_list : JsonEncState,
    U64,
    (JsonEncBox, (JsonEncBox, (JsonEncState -> Try(JsonEncState, err)) -> Try(JsonEncBox, err)) -> Try(JsonEncBox, err)) -> Try(JsonEncState, err)
    encode_list = |state, count, write_items| JsonEnc.encode_tuple(state, count, write_items)

    ## A tuple encodes as a JSON array, same as a list.
    encode_tuple : JsonEncState,
    U64,
    (JsonEncBox, (JsonEncBox, (JsonEncState -> Try(JsonEncState, err)) -> Try(JsonEncBox, err)) -> Try(JsonEncBox, err)) -> Try(JsonEncState, err)
    encode_tuple = |state, _count, write_items| {
        opened = JsonEncBox.{ output: state.output.append(91), needs_comma: False }
        finished = write_items(opened, JsonEnc.write_item)?
        Ok(JsonEncState.{ output: finished.output.append(93) })
    }

    ## Internal (associated so the container writers can be passed by name):
    ## one record field — optional comma, quoted name, colon, then the value.
    write_field : JsonEncBox, Str, (JsonEncState -> Try(JsonEncState, err)) -> Try(JsonEncBox, err)
    write_field = |box, name, write_value| {
        separated = if box.needs_comma box.output.append(44) else box.output
        named = quoted(separated, name).append(58)
        encoded = write_value(JsonEncState.{ output: named })?
        Ok(JsonEncBox.{ output: encoded.output, needs_comma: True })
    }

    ## Internal: one array element — optional comma, then the value.
    write_item : JsonEncBox, (JsonEncState -> Try(JsonEncState, err)) -> Try(JsonEncBox, err)
    write_item = |box, write_value| {
        separated = if box.needs_comma box.output.append(44) else box.output
        encoded = write_value(JsonEncState.{ output: separated })?
        Ok(JsonEncBox.{ output: encoded.output, needs_comma: True })
    }

    ## Encode `value` straight to UTF-8 bytes — the whole point of owning the
    ## format. `capacity` seeds the buffer; the builtin hardcodes 64.
    ##
    ## This is the ONLY encode entry point on purpose. A thin
    ## `to_utf8 = |v| JsonEnc.to_utf8_with_capacity(v, default_capacity)`
    ## wrapper type-checks, passes its expects, and then SEGFAULTS at runtime
    ## when called from a platform nominal's method (`Res.json`) — the
    ## "generic nominal-module method forwarding to a platform generic" family
    ## already recorded in notes/2026-07-16-compiler-generic-nominal-method-
    ## forward-segfault.md. Call this directly; do not reintroduce a wrapper.
    to_utf8_with_capacity : a, U64 -> List(U8)
        where [a.encoder_for : JsonEnc -> (a, JsonEncState -> Try(JsonEncState, []))]
    to_utf8_with_capacity = |value, capacity| {
        Shape : a
        encode_value = Shape.encoder_for(JsonEnc.Default)
        # Refutable destructure, as the builtin's `Json.to_str` does: the error
        # row is empty (`[]`), so `Ok` is the only inhabited case. Writing this
        # as a `match` with an `Err(_) => []` arm CRASHES at runtime — the arm
        # is uninhabited, not merely unreachable.
        Ok(encoded) = encode_value(value, JsonEncState.{ output: List.with_capacity(capacity) })
        encoded.output
    }

    ## The encoded form as text. Only for tests and for callers that genuinely
    ## want a `Str`; response bodies should use `to_utf8`.
    to_str : a -> Str
        where [a.encoder_for : JsonEnc -> (a, JsonEncState -> Try(JsonEncState, []))]
    to_str = |value| Str.from_utf8_lossy(JsonEnc.to_utf8_with_capacity(value, default_capacity))

    ## --- Streaming primitives -------------------------------------------------
    ##
    ## Append one JSON value straight onto a `List(U8)` buffer, for a
    ## hand-written serializer that fuses reading with encoding and never
    ## materializes an intermediate record (the derived `encoder_for` walks a
    ## value; these let a caller walk a cursor instead). Same quoting/escaping
    ## as the derived path.

    ## Append a quoted, escaped JSON string.
    str_into : List(U8), Str -> List(U8)
    str_into = |out, value| quoted(out, value)

    ## Append a JSON integer.
    i64_into : List(U8), I64 -> List(U8)
    i64_into = |out, value| raw_bytes(out, I64.to_str(value))

    ## Append JSON `true`/`false`.
    bool_into : List(U8), Bool -> List(U8)
    bool_into = |out, value| raw_bytes(out, if value "true" else "false")

    ## Append JSON `null`.
    null_into : List(U8) -> List(U8)
    null_into = |out| raw_bytes(out, "null")

    ## Append a run of raw, pre-formed bytes (structural punctuation, constant
    ## keys). The caller guarantees it is valid JSON.
    raw_into : List(U8), Str -> List(U8)
    raw_into = |out, literal| raw_bytes(out, literal)
}

## The encoder's output buffer. Owning this type is what lets `Res.json` hand
## the response its bytes without a `Str` in between.
JsonEncState := { output : List(U8) }

## Container state: the buffer plus whether a separator is owed. Kept distinct
## from `JsonEncState` so a value writer cannot accidentally see `needs_comma`.
JsonEncBox := { output : List(U8), needs_comma : Bool }

## Starting buffer size. The builtin hardcodes 64; response bodies are
## routinely kilobytes, and growth is the expensive part.
default_capacity : U64
default_capacity = 1024

## Internal: append a string's UTF-8 bytes verbatim (no quoting/escaping) —
## for numbers, `true`/`false`, `null`.
##
## `to_utf8()` allocates a temporary byte list. The builtin avoids it with
## compiler-only primitives (`u8_list_reserve` + `str_get_utf8_byte_unsafe`)
## that userland cannot reach; this is the vendoring's one real cost.
raw_bytes : List(U8), Str -> List(U8)
raw_bytes = |out, text| out.concat(text.to_utf8())

## Internal: append a quoted, escaped JSON string.
##
## Fast path first: a string needing no escaping (the overwhelming majority —
## slugs, usernames, timestamps, tags) is appended whole, so the per-byte walk
## only runs for strings that actually contain a quote, a backslash, or a
## control character.
quoted : List(U8), Str -> List(U8)
quoted = |out, text| {
    bytes = text.to_utf8()
    opened = out.append(34)
    body = if needs_escaping(bytes) escaped(opened, bytes) else opened.concat(bytes)
    body.append(34)
}

## Internal: does this string contain anything JSON requires us to escape?
needs_escaping : List(U8) -> Bool
needs_escaping = |bytes| bytes.any(|byte| byte == 34 or byte == 92 or byte < 32)

## Internal: the escaping walk. Only reached for strings that need it.
escaped : List(U8), List(U8) -> List(U8)
escaped = |out, bytes| bytes.fold(out, |acc, byte| {
    if byte == 34 {
        acc.append(92).append(34)
    } else if byte == 92 {
        acc.append(92).append(92)
    } else if byte == 10 {
        acc.append(92).append(110)
    } else if byte == 13 {
        acc.append(92).append(114)
    } else if byte == 9 {
        acc.append(92).append(116)
    } else if byte == 8 {
        acc.append(92).append(98)
    } else if byte == 12 {
        acc.append(92).append(102)
    } else if byte < 32 {
        # \u00XX for the remaining control characters.
        acc.append(92).append(117).append(48).append(48).append(hex_digit(byte / 16)).append(hex_digit(byte % 16))
    } else {
        acc.append(byte)
    }
})

## Internal: one lowercase hex digit as its ASCII byte.
hex_digit : U8 -> U8
hex_digit = |nibble| if nibble < 10 (48 + nibble) else (87 + nibble)

## a record encodes with quoted keys and no spaces
expect JsonEnc.to_str({ id: 7.U64, name: "x" }) == "{\"id\":7,\"name\":\"x\"}"

## a nested record encodes inline
expect JsonEnc.to_str({ outer: { inner: 1.I64 } }) == "{\"outer\":{\"inner\":1}}"

## a list of records encodes as a JSON array
expect JsonEnc.to_str({ items: [{ n: 1.I64 }, { n: 2.I64 }] }) == "{\"items\":[{\"n\":1},{\"n\":2}]}"

## an empty list encodes as []
expect {
    items : List(I64)
    items = []
    JsonEnc.to_str({ items: items }) == "{\"items\":[]}"
}

# NOTE: a raw `Bool` record field is NOT encodable here, and must not be. The
# derived encoder routes a `Bool` through the tag-union path rather than
# `encode_bool` (the long-standing gap in
# notes/2026-07-12-compiler-bool-encode-gap.md, which is why `JsonBool`
# exists). Against the builtin that produced the string "True"; against this
# format, which deliberately has no `encode_tag`, it CRASHES at runtime rather
# than failing to compile. Response bodies use `JsonBool`, whose encoder calls
# `encode_bool` directly — covered by JsonBool.roc's own expects.

## a negative integer keeps its sign
expect JsonEnc.to_str({ n: -42.I64 }) == "{\"n\":-42}"

## quotes and backslashes are escaped
expect JsonEnc.to_str({ s: "a\"b\\c" }) == "{\"s\":\"a\\\"b\\\\c\"}"

## newline, carriage return and tab take their short escapes
expect JsonEnc.to_str({ s: "a\nb\rc\td" }) == "{\"s\":\"a\\nb\\rc\\td\"}"

## other control characters take the \u00XX form
expect JsonEnc.to_str({ s: Str.from_utf8_lossy([1]) }) == "{\"s\":\"\\u0001\"}"

## a string needing no escaping survives the fast path unchanged
expect JsonEnc.to_str({ s: "plain text, 123" }) == "{\"s\":\"plain text, 123\"}"

## multi-byte UTF-8 passes through as-is (JSON allows raw UTF-8)
expect JsonEnc.to_str({ s: "héllo" }) == "{\"s\":\"héllo\"}"

## to_utf8 yields the bytes directly, with no Str in between
expect JsonEnc.to_utf8_with_capacity({ n: 1.U64 }, default_capacity) == "{\"n\":1}".to_utf8()

## the capacity seed does not change the output
expect JsonEnc.to_utf8_with_capacity({ n: 1.U64 }, 4) == "{\"n\":1}".to_utf8()
