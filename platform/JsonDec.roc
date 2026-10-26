## The platform's own JSON DECODING — a userland `Encoding` format, the decode
## companion to `JsonEnc`. Vendored for the same reason: `Builtin.Encoding.
## JsonEncoding` is not reachable from user code, so the platform cannot
## delegate to it.
##
## Why own the decoder when the builtin `Json.parse` already works: error
## QUALITY. The builtin collapses a wrong-typed field into the detail-free
## `InvalidJson("Invalid JSON")`. This format knows the field name at the
## moment a value is parsed (the driver hands it over in `parse_record_field`),
## so a mistyped field is reported as `InvalidField(name, expected)`, and
## because a scalar mismatch RECORDS into the format's own state and returns a
## fabricated value rather than aborting, several mistyped fields ACCUMULATE
## into one response instead of stopping at the first.
##
## Scope: the shapes the platform's bodies actually decode — strings, records
## (nested, with optional `Try(_, [Missing])` fields), JSON `null` (for
## `NullableStr`), and `List(Str)`. Numbers, bools, dicts and tuples are NOT
## implemented; a body decoding one is a COMPILE error naming the missing
## method, which is the signal to extend this deliberately.
JsonDec := [Default].{
    ## Field names arrive from the wire unchanged.
    rename_field : JsonDec, Str -> Str
    rename_field = |_, name| name

    ## A JSON string. On a non-string value this is a type mismatch: record
    ## `InvalidField(field, "a string")`, consume the offending value, and
    ## return a fabricated "" so the parse continues and later mismatches also
    ## accumulate. The fabricated value never escapes — the caller rejects on a
    ## non-empty `errors` before any handler runs.
    parse_str : JsonDec, JsonDecState -> Try({ value : Str, rest : JsonDecState }, [InvalidJson(Str)])
    parse_str = |_, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, "\"") {
            scanned = take_json_string(trimmed)?
            Ok({ value: scanned.value, rest: with_input(state, scanned.rest) })
        } else {
            consumed = skip_value(trimmed)
            noted = record_error(state, InvalidField(state.field, "a string"))
            Ok({ value: "", rest: with_input(noted, consumed) })
        }
    }

    ## JSON `null`. Used by `NullableStr`, which tries this first and falls back
    ## to `parse_str`; so a FAILURE here must be a plain `Err` (no error
    ## recorded) to let that fallback run.
    parse_null : JsonDec, JsonDecState -> Try(JsonDecState, [InvalidJson(Str)])
    parse_null = |_, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, "null") {
            Ok(with_input(state, Str.drop_prefix(trimmed, "null")))
        } else {
            Err(InvalidJson("expected null"))
        }
    }

    ## A JSON number as I64. Reads the bare scalar and parses it. A
    ## non-numeric value is a mismatch: record `InvalidField`, consume, and
    ## fabricate 0 — the same accumulation contract as `parse_str`.
    parse_i64 : JsonDec, JsonDecState -> Try({ value : I64, rest : JsonDecState }, [InvalidJson(Str)])
    parse_i64 = |_, state| {
        trimmed = skip_ws(state.input)
        token = take_scalar(trimmed)
        match I64.from_str(token.value) {
            Ok(n) => Ok({ value: n, rest: with_input(state, token.rest) })
            Err(_) => {
                noted = record_error(state, InvalidField(state.field, "an integer"))
                Ok({ value: 0, rest: with_input(noted, skip_value(trimmed)) })
            }
        }
    }

    ## A JSON number as U64, same contract as `parse_i64`.
    parse_u64 : JsonDec, JsonDecState -> Try({ value : U64, rest : JsonDecState }, [InvalidJson(Str)])
    parse_u64 = |_, state| {
        trimmed = skip_ws(state.input)
        token = take_scalar(trimmed)
        match U64.from_str(token.value) {
            Ok(n) => Ok({ value: n, rest: with_input(state, token.rest) })
            Err(_) => {
                noted = record_error(state, InvalidField(state.field, "a non-negative integer"))
                Ok({ value: 0, rest: with_input(noted, skip_value(trimmed)) })
            }
        }
    }

    ## An object opens with `{`. No count is knowable up front, so Uncounted.
    parse_record_start : JsonDec, JsonDecState -> Try([Counted({ len : U64, rest : JsonDecState }), Uncounted(JsonDecState)], [InvalidJson(Str)])
    parse_record_start = |_, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, "{") {
            Ok(Uncounted(with_input(state, Str.drop_prefix(trimmed, "{"))))
        } else {
            Err(InvalidJson("expected object"))
        }
    }

    ## Read the next `"key":` and NAME it to the driver, leaving `input` at the
    ## value and `field` set so a scalar parser can report against it. `}` ends
    ## the object.
    parse_record_field : JsonDec,
    Encoding.FieldName.FieldNames(_shape),
    JsonDecState -> Try(
        [
            Field({ field : Encoding.FieldName(_shape), rest : JsonDecState }),
            TryField({ name : Str, rest : JsonDecState }),
            TryFieldCaseless({ name : Str, rest : JsonDecState }),
            Continue(JsonDecState),
            Done(JsonDecState),
        ],
        [InvalidJson(Str)],
    )
    parse_record_field = |_, _fields, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, "}") {
            Ok(Done(with_input(state, Str.drop_prefix(trimmed, "}"))))
        } else if Str.starts_with(trimmed, "\"") {
            scanned = take_json_string(trimmed)?
            after_colon = skip_ws(scanned.rest)
            if Str.starts_with(after_colon, ":") {
                value_at = Str.drop_prefix(after_colon, ":")
                Ok(TryField({ name: scanned.value, rest: { input: value_at, field: scanned.value, errors: state.errors } }))
            } else {
                Err(InvalidJson("expected ':' after object key"))
            }
        } else {
            Err(InvalidJson("expected object key or '}'"))
        }
    }

    ## After a value, a `,` continues and `}` ends. The comma is consumed here
    ## so the next `parse_record_field` starts on a key.
    parse_record_after_field : JsonDec, JsonDecState -> Try([Continue(JsonDecState), Done(JsonDecState)], [InvalidJson(Str)])
    parse_record_after_field = |_, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, ",") {
            Ok(Continue(with_input(state, Str.drop_prefix(trimmed, ","))))
        } else if Str.starts_with(trimmed, "}") {
            Ok(Done(with_input(state, Str.drop_prefix(trimmed, "}"))))
        } else {
            Err(InvalidJson("expected ',' or '}'"))
        }
    }

    ## An undemanded field's value — consume one JSON value and move on.
    skip_record_field : JsonDec, JsonDecState -> Try(JsonDecState, [InvalidJson(Str)])
    skip_record_field = |_, state| Ok(with_input(state, skip_value(skip_ws(state.input))))

    ## A JSON array opens with `[`.
    parse_list_start : JsonDec, JsonDecState -> Try([Counted({ len : U64, rest : JsonDecState }), Uncounted(JsonDecState)], [InvalidJson(Str)])
    parse_list_start = |_, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, "[") {
            Ok(Uncounted(with_input(state, Str.drop_prefix(trimmed, "["))))
        } else {
            Err(InvalidJson("expected array"))
        }
    }

    ## Before each element: `]` ends the array, otherwise an Item follows.
    parse_list_next : JsonDec, JsonDecState -> Try([Item(JsonDecState), Done(JsonDecState)], [InvalidJson(Str)])
    parse_list_next = |_, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, "]") {
            Ok(Done(with_input(state, Str.drop_prefix(trimmed, "]"))))
        } else {
            Ok(Item(with_input(state, trimmed)))
        }
    }

    ## After an element: `,` continues, `]` ends.
    parse_list_after_item : JsonDec, JsonDecState -> Try([Continue(JsonDecState), Done(JsonDecState)], [InvalidJson(Str)])
    parse_list_after_item = |_, state| {
        trimmed = skip_ws(state.input)
        if Str.starts_with(trimmed, ",") {
            Ok(Continue(with_input(state, Str.drop_prefix(trimmed, ","))))
        } else if Str.starts_with(trimmed, "]") {
            Ok(Done(with_input(state, Str.drop_prefix(trimmed, "]"))))
        } else {
            Err(InvalidJson("expected ',' or ']'"))
        }
    }

    invalid_value : JsonDec, JsonDecState -> [InvalidJson(Str)]
    invalid_value = |_, _state| InvalidJson("invalid JSON value")

    ## Decode `text` into whatever shape the caller's annotation/accesses fix,
    ## returning the accumulated field mismatches alongside. A structural
    ## failure (malformed JSON, an absent required field) short-circuits
    ## through the driver's `Err`; a type mismatch on a present field lands in
    ## `errors` with the value fabricated.
    parse : Str -> Try({ value : a, errors : List([InvalidField(Str, Str), InvalidJson(Str), MissingRequiredField(Str)]) }, [InvalidJson(Str), MissingRequiredField(Str), ..errs])
        where [
            a.parser_for : JsonDec -> (JsonDecState -> Try({ value : a, rest : JsonDecState }, [InvalidJson(Str), MissingRequiredField(Str), ..errs])),
        ]
    parse = |text| {
        Shape : a
        parse_shape = Shape.parser_for(JsonDec.Default)
        parsed = parse_shape({ input: text, field: "", errors: [] })?
        Ok({ value: parsed.value, errors: parsed.rest.errors })
    }
}

## The decoder's cursor: the remaining unparsed text, the name of the field
## currently being read (for `InvalidField`), and the accumulated mismatches.
JsonDecState := { input : Str, field : Str, errors : List([InvalidField(Str, Str), InvalidJson(Str), MissingRequiredField(Str)]) }

## Internal: replace the cursor's remaining text, keeping field and errors.
with_input : JsonDecState, Str -> JsonDecState
with_input = |state, input| { input: input, field: state.field, errors: state.errors }

## Internal: append a mismatch to the cursor's accumulator.
record_error : JsonDecState, [InvalidField(Str, Str), InvalidJson(Str), MissingRequiredField(Str)] -> JsonDecState
record_error = |state, err| { input: state.input, field: state.field, errors: state.errors.append(err) }

## Internal: drop leading JSON whitespace.
skip_ws : Str -> Str
skip_ws = |s| {
    bytes = s.to_utf8()
    kept = drop_ws_bytes(bytes, 0)
    Str.from_utf8_lossy(List.sublist(bytes, { start: kept, len: bytes.len() - kept }))
}

## Internal: index of the first non-whitespace byte.
drop_ws_bytes : List(U8), U64 -> U64
drop_ws_bytes = |bytes, i| {
    when_ws = match bytes.get(i) {
        Ok(b) => b == 32 or b == 9 or b == 10 or b == 13
        Err(_) => False
    }
    if when_ws drop_ws_bytes(bytes, i + 1) else i
}

## Internal: read a `"..."` JSON string (assumes input starts with `"`),
## decoding escapes, returning the value and the remaining text after the
## closing quote.
take_json_string : Str -> Try({ value : Str, rest : Str }, [InvalidJson(Str)])
take_json_string = |s| {
    bytes = s.to_utf8()
    # skip the opening quote (byte 0)
    scan_string(bytes, 1, [])
}

## Internal: the escape-aware scan. `out` accumulates decoded bytes.
scan_string : List(U8), U64, List(U8) -> Try({ value : Str, rest : Str }, [InvalidJson(Str)])
scan_string = |bytes, i, out| {
    match bytes.get(i) {
        Err(_) => Err(InvalidJson("unterminated string"))
        Ok(byte) =>
            if byte == 34 {
                # closing quote
                rest_bytes = List.sublist(bytes, { start: i + 1, len: bytes.len() - (i + 1) })
                Ok({ value: Str.from_utf8_lossy(out), rest: Str.from_utf8_lossy(rest_bytes) })
            } else if byte == 92 {
                # backslash: decode one escape
                esc = decode_escape(bytes, i + 1)?
                scan_string(bytes, esc.next, out.concat(esc.bytes))
            } else {
                scan_string(bytes, i + 1, out.append(byte))
            }
    }
}

## Internal: decode one escape sequence starting just after the backslash.
## Returns the decoded bytes and the index just past the sequence.
decode_escape : List(U8), U64 -> Try({ bytes : List(U8), next : U64 }, [InvalidJson(Str)])
decode_escape = |bytes, i| {
    match bytes.get(i) {
        Err(_) => Err(InvalidJson("unterminated escape"))
        Ok(byte) =>
            if byte == 34 {
                Ok({ bytes: [34], next: i + 1 })
            } else if byte == 92 {
                Ok({ bytes: [92], next: i + 1 })
            } else if byte == 47 {
                Ok({ bytes: [47], next: i + 1 })
            } else if byte == 110 {
                Ok({ bytes: [10], next: i + 1 })
            } else if byte == 116 {
                Ok({ bytes: [9], next: i + 1 })
            } else if byte == 114 {
                Ok({ bytes: [13], next: i + 1 })
            } else if byte == 98 {
                Ok({ bytes: [8], next: i + 1 })
            } else if byte == 102 {
                Ok({ bytes: [12], next: i + 1 })
            } else if byte == 117 {
                decode_unicode_escape(bytes, i + 1)
            } else {
                Err(InvalidJson("invalid escape"))
            }
    }
}

## Internal: decode a `\uXXXX` escape (i points at the first hex digit),
## emitting UTF-8 bytes for the code point.
decode_unicode_escape : List(U8), U64 -> Try({ bytes : List(U8), next : U64 }, [InvalidJson(Str)])
decode_unicode_escape = |bytes, i| {
    h0 = hex_at(bytes, i)?
    h1 = hex_at(bytes, i + 1)?
    h2 = hex_at(bytes, i + 2)?
    h3 = hex_at(bytes, i + 3)?
    code = h0.to_u64() * 4096 + h1.to_u64() * 256 + h2.to_u64() * 16 + h3.to_u64()
    Ok({ bytes: utf8_encode(code), next: i + 4 })
}

## Internal: one hex digit's value at index i.
hex_at : List(U8), U64 -> Try(U8, [InvalidJson(Str)])
hex_at = |bytes, i| match bytes.get(i) {
    Err(_) => Err(InvalidJson("truncated \\u escape"))
    Ok(b) =>
        if b >= 48 and b <= 57 {
            Ok(b - 48)
        } else if b >= 97 and b <= 102 {
            Ok(b - 87)
        } else if b >= 65 and b <= 70 {
            Ok(b - 55)
        } else {
            Err(InvalidJson("bad hex digit in \\u escape"))
        }
}

## Internal: UTF-8 encode a code point in the Basic Multilingual Plane. (JSON
## surrogate pairs for astral characters are not handled — none appear in the
## platform's decode inputs; a lone surrogate encodes literally.)
utf8_encode : U64 -> List(U8)
utf8_encode = |code| {
    if code < 128 {
        [code.to_u8_wrap()]
    } else if code < 2048 {
        [(192 + code / 64).to_u8_wrap(), (128 + code % 64).to_u8_wrap()]
    } else {
        [(224 + code / 4096).to_u8_wrap(), (128 + (code / 64) % 64).to_u8_wrap(), (128 + code % 64).to_u8_wrap()]
    }
}

## Internal: consume exactly one JSON value from the front of `s` (already
## whitespace-trimmed), returning the remaining text. Used to skip undemanded
## fields and mistyped values. Handles strings, null/true/false, numbers, and
## nested arrays/objects.
skip_value : Str -> Str
skip_value = |s| {
    if Str.starts_with(s, "\"") {
        match take_json_string(s) {
            Ok(scanned) => scanned.rest
            Err(_) => ""
        }
    } else if Str.starts_with(s, "{") {
        skip_container(Str.drop_prefix(s, "{"), 125, 123)
    } else if Str.starts_with(s, "[") {
        skip_container(Str.drop_prefix(s, "["), 93, 91)
    } else if Str.starts_with(s, "null") {
        Str.drop_prefix(s, "null")
    } else if Str.starts_with(s, "true") {
        Str.drop_prefix(s, "true")
    } else if Str.starts_with(s, "false") {
        Str.drop_prefix(s, "false")
    } else {
        # a number or bare token: drop up to the next structural byte
        drop_scalar(s)
    }
}

## Internal: skip to just past the matching close byte, respecting nesting and
## strings. `close`/`open` are the ASCII bytes for this container.
skip_container : Str, U8, U8 -> Str
skip_container = |s, close, open| {
    bytes = s.to_utf8()
    end = scan_container(bytes, 0, 1, close, open)
    Str.from_utf8_lossy(List.sublist(bytes, { start: end, len: bytes.len() - end }))
}

## Internal: index just past the byte that closes depth 0.
scan_container : List(U8), U64, U64, U8, U8 -> U64
scan_container = |bytes, i, depth, close, open| {
    match bytes.get(i) {
        Err(_) => i
        Ok(byte) =>
            if byte == 34 {
                scan_container(bytes, skip_string_bytes(bytes, i + 1), depth, close, open)
            } else if byte == open {
                scan_container(bytes, i + 1, depth + 1, close, open)
            } else if byte == close {
                if depth == 1 (i + 1) else scan_container(bytes, i + 1, depth - 1, close, open)
            } else {
                scan_container(bytes, i + 1, depth, close, open)
            }
    }
}

## Internal: index just past a string's closing quote (i starts inside the
## string, after the opening quote), skipping escaped quotes.
skip_string_bytes : List(U8), U64 -> U64
skip_string_bytes = |bytes, i| match bytes.get(i) {
    Err(_) => i
    Ok(byte) =>
        if byte == 34 {
            i + 1
        } else if byte == 92 {
            skip_string_bytes(bytes, i + 2)
        } else {
            skip_string_bytes(bytes, i + 1)
        }
}

## Internal: drop a bare scalar (number) up to the next structural/whitespace
## byte.
drop_scalar : Str -> Str
drop_scalar = |s| {
    bytes = s.to_utf8()
    end = scalar_end(bytes, 0)
    Str.from_utf8_lossy(List.sublist(bytes, { start: end, len: bytes.len() - end }))
}

## Internal: split a bare scalar token (a number) off the front of `s`.
take_scalar : Str -> { value : Str, rest : Str }
take_scalar = |s| {
    bytes = s.to_utf8()
    end = scalar_end(bytes, 0)
    { value: Str.from_utf8_lossy(List.sublist(bytes, { start: 0, len: end })),
      rest: Str.from_utf8_lossy(List.sublist(bytes, { start: end, len: bytes.len() - end })) }
}

## Internal: index of the first byte that ends a bare scalar.
scalar_end : List(U8), U64 -> U64
scalar_end = |bytes, i| match bytes.get(i) {
    Err(_) => i
    Ok(byte) =>
        if byte == 44 or byte == 125 or byte == 93 or byte == 32 or byte == 9 or byte == 10 or byte == 13 {
            i
        } else {
            scalar_end(bytes, i + 1)
        }
}

# --- string scanner ---

## a plain string decodes and the rest is returned
expect take_json_string("\"hi\" , rest") == Ok({ value: "hi", rest: " , rest" })

## an escaped quote and backslash decode
expect take_json_string("\"a\\\"b\\\\c\"") == Ok({ value: "a\"b\\c", rest: "" })

## short escapes decode
expect take_json_string("\"a\\nb\\tc\"") == Ok({ value: "a\nb\tc", rest: "" })

## a \u escape decodes to UTF-8
expect take_json_string("\"\\u00e9\"") == Ok({ value: "é", rest: "" })

## skip_value consumes a nested object
expect skip_value("{\"a\":{\"b\":1},\"c\":2} tail") == " tail"

## skip_value consumes an array with strings containing braces
expect skip_value("[\"}\",\"]\"] tail") == " tail"

# --- whole-record decode via the derived driver ---

## a flat record decodes
expect {
    decoded : Try({ value : { name : Str }, errors : _ }, _)
    decoded = JsonDec.parse("{\"name\":\"ada\"}")
    match decoded {
        Ok(r) => r.value.name == "ada" and r.errors.is_empty()
        Err(_) => False
    }
}

## a nested envelope decodes and ignores an undemanded field
expect {
    decoded : Try({ value : { user : { email : Str } }, errors : _ }, _)
    decoded = JsonDec.parse("{\"user\":{\"email\":\"j@x.com\",\"extra\":\"ignored\"}}")
    match decoded {
        Ok(r) => r.value.user.email == "j@x.com"
        Err(_) => False
    }
}

## a list of strings decodes
expect {
    decoded : Try({ value : { tags : List(Str) }, errors : _ }, _)
    decoded = JsonDec.parse("{\"tags\":[\"a\",\"b\",\"c\"]}")
    match decoded {
        Ok(r) => r.value.tags == ["a", "b", "c"]
        Err(_) => False
    }
}

## an absent optional field is Err(Missing)
expect {
    decoded : Try({ value : { name : Str, nick : Try(Str, _) }, errors : _ }, _)
    decoded = JsonDec.parse("{\"name\":\"ada\"}")
    match decoded {
        Ok(r) => r.value.nick == Err(Missing)
        Err(_) => False
    }
}

## an absent required field short-circuits with its name
expect {
    decoded : Try({ value : { name : Str }, errors : _ }, _)
    decoded = JsonDec.parse("{\"other\":\"x\"}")
    decoded == Err(MissingRequiredField("name"))
}

## two mistyped fields BOTH accumulate, and the parse still completes
expect {
    decoded : Try({ value : { a : Str, b : Str }, errors : _ }, _)
    decoded = JsonDec.parse("{\"a\":1,\"b\":2}")
    match decoded {
        Ok(r) => r.errors == [InvalidField("a", "a string"), InvalidField("b", "a string")]
        Err(_) => False
    }
}

## a numeric field decodes, and a mistyped numeric field accumulates
expect {
    decoded : Try({ value : { id : I64 }, errors : _ }, _)
    decoded = JsonDec.parse("{\"id\":42}")
    match decoded {
        Ok(r) => r.value.id == 42 and r.errors.is_empty()
        Err(_) => False
    }
}

## a string where an integer is demanded is named and accumulated
expect {
    decoded : Try({ value : { id : I64 }, errors : _ }, _)
    decoded = JsonDec.parse("{\"id\":\"nope\"}")
    match decoded {
        Ok(r) => r.errors == [InvalidField("id", "an integer")]
        Err(_) => False
    }
}
