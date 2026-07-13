## An I64 that JSON-encodes as a real number or `null`. The nullable-integer
## companion to `NullableStr`, emitted by sqlgen for a nullable INTEGER column
## (`I64?`). A record field typed `NullableI64` comes out of `Res.json` /
## `Json.to_str` as `"n":7` or `"n":null`.
NullableI64 := [Null, Val(I64)].{
    ## The `Json.to_str` hook: `Null` encodes as JSON `null`, `Val(n)` as a
    ## JSON number.
    encoder_for = |encoding| |nullable, state| match nullable {
        Null => Json.encode_null(encoding, state)
        Val(inner) => Json.encode_i64(encoding, inner, state)
    }

    ## The `Json.parse` hook: JSON `null` decodes to `Null`, a number to `Val`.
    parser_for = |encoding| |state| match Json.parse_null(encoding, state) {
        Ok(rest) => Ok({ value: NullableI64.Null, rest: rest })
        Err(_) => match Json.parse_i64(encoding, state) {
            Ok({ value, rest }) => Ok({ value: NullableI64.Val(value), rest: rest })
            Err(problem) => Err(problem)
        }
    }

    ## The contained integer, or `default` when null.
    with_default : NullableI64, I64 -> I64
    with_default = |nullable, default| match nullable {
        Val(value) => value
        Null => default
    }
}

## A Null field encodes as JSON null
expect {
    n : NullableI64
    n = NullableI64.Null
    Json.to_str({ n: n }) == "{\"n\":null}"
}

## A Val field encodes as a plain JSON number
expect {
    n : NullableI64
    n = NullableI64.Val(7)
    Json.to_str({ n: n }) == "{\"n\":7}"
}

## with_default returns the contained value for Val
expect NullableI64.with_default(NullableI64.Val(3), 0) == 3

## with_default falls back for Null
expect NullableI64.with_default(NullableI64.Null, 0) == 0

## explicit JSON null decodes to Null
expect {
    parsed : Try({ n : NullableI64 }, Json.ParseErr)
    parsed = Json.parse("{\"n\":null}")
    parsed == Ok({ n: NullableI64.Null })
}

## a JSON number decodes to Val
expect {
    parsed : Try({ n : NullableI64 }, Json.ParseErr)
    parsed = Json.parse("{\"n\":5}")
    parsed == Ok({ n: NullableI64.Val(5) })
}
