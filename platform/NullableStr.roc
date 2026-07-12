## A string that JSON-encodes as real `null` when absent. The derived
## encoding for tag unions renders `None` as the STRING `"None"`, which no
## JSON API wants; this nominal supplies its own `encoder_for`, so a record
## field typed `NullableStr` comes out of `Res.json` / `Json.to_str` as
## `"bio":null` or `"bio":"text"`.
##
## Output-only: there is no matching `parser_for`, so don't use it in
## `Req.json_body` shapes — decode optional INPUT fields as
## `Try(Str, [Missing])` instead (absent fields parse to `Err(Missing)`).
NullableStr := [Null, Val(Str)].{
    ## The `Json.to_str` hook: `Null` encodes as JSON `null`, `Val(s)` as a
    ## JSON string. Derived record/list encoders pick this up for any field
    ## typed `NullableStr`.
    encoder_for = |encoding| |nullable, state| match nullable {
        Null => Json.encode_null(encoding, state)
        Val(inner) => Json.encode_str(encoding, inner, state)
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
