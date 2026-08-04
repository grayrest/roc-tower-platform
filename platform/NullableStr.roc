## A string that JSON-encodes as real `null` when absent. The derived
## encoding for tag unions renders `None` as the STRING `"None"`, which no
## JSON API wants; this nominal supplies its own `encoder_for`, so a record
## field typed `NullableStr` comes out of `Res.json` / `Json.to_str` as
## `"bio":null` or `"bio":"text"`.
##
## Decoding works too: a `NullableStr` field accepts JSON `null` or a
## string, and `Try(NullableStr, [Missing])` distinguishes all THREE input
## states of a nullable PATCH field — absent (`Err(Missing)`), explicit
## null (`Ok(Null)`), and a value (`Ok(Val(s))`).
NullableStr := [Null, Val(Str)].{
    ## Structural equality. A nominal that declares associated items no longer
    ## receives a derived `is_eq`, so `==` on a `NullableStr` needs this
    ## spelled out (it is what the expects below, and any app comparing a
    ## nullable column, rely on).
    is_eq : NullableStr, NullableStr -> Bool
    is_eq = |left, right| match (left, right) {
        (Null, Null) => True
        (Val(left_text), Val(right_text)) => left_text == right_text
        _ => False
    }

    ## The `Json.to_str` hook: `Null` encodes as JSON `null`, `Val(s)` as a
    ## JSON string. Derived record/list encoders pick this up for any field
    ## typed `NullableStr`.
    encoder_for = |encoding| |nullable, state| match nullable {
        Null => Json.encode_null(encoding, state)
        Val(inner) => Json.encode_str(encoding, inner, state)
    }

    ## The `Json.parse` hook: JSON `null` decodes to `Null`, a string to
    ## `Val`; anything else is that value's parse error.
    parser_for = |encoding| |state| match Json.parse_null(encoding, state) {
        Ok(rest) => Ok({ value: NullableStr.Null, rest: rest })
        Err(_) => match Json.parse_str(encoding, state) {
            Ok({ value, rest }) => Ok({ value: NullableStr.Val(value), rest: rest })
            Err(problem) => Err(problem)
        }
    }

    ## Collapse a Try whose Err means "absent" — a nullable SQL cell, a
    ## missing JSON field — into a NullableStr.
    from_try : Try(Str, err) -> NullableStr
    from_try = |result| match result {
        Ok(value) => Val(value)
        Err(_) => Null
    }

    ## The contained string, or `default` when null.
    with_default : NullableStr, Str -> Str
    with_default = |nullable, default| match nullable {
        Val(value) => value
        Null => default
    }
}

## A Null field encodes as JSON null (not the string "None")
expect {
    bio : NullableStr
    bio = NullableStr.Null
    Json.to_str({ bio: bio }) == "{\"bio\":null}"
}

## A Val field encodes as a plain JSON string
expect {
    bio : NullableStr
    bio = NullableStr.Val("hi")
    Json.to_str({ bio: bio }) == "{\"bio\":\"hi\"}"
}

## from_try maps Ok to Val
expect NullableStr.from_try(Ok("x")) == NullableStr.Val("x")

## from_try maps any Err to Null
expect NullableStr.from_try(Err(Missing)) == NullableStr.Null

## with_default returns the contained string for Val
expect NullableStr.with_default(NullableStr.Val("a"), "d") == "a"

## with_default falls back for Null
expect NullableStr.with_default(NullableStr.Null, "d") == "d"

## explicit JSON null decodes to Null
expect {
    parsed : Try({ bio : NullableStr }, _)
    parsed = Json.parse("{\"bio\":null}")
    parsed == Ok({ bio: NullableStr.Null })
}

## a JSON string decodes to Val
expect {
    parsed : Try({ bio : NullableStr }, _)
    parsed = Json.parse("{\"bio\":\"hi\"}")
    parsed == Ok({ bio: NullableStr.Val("hi") })
}

## tri-state PATCH decode: absent field is Err(Missing), explicit null is Ok(Null)
expect {
    absent : Try({ bio : Try(NullableStr, [Missing]) }, _)
    absent = Json.parse("{}")
    explicit : Try({ bio : Try(NullableStr, [Missing]) }, _)
    explicit = Json.parse("{\"bio\":null}")
    absent == Ok({ bio: Err(Missing) }) and explicit == Ok({ bio: Ok(NullableStr.Null) })
}
