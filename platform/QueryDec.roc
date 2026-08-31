## The platform's own QUERY-STRING decoding — a userland `Encoding` format that
## decodes `a=1&b=two%20words` into a record whose field set is the query
## parameters a handler asks for. The query analogue of `JsonDec`: same
## protocol, same accumulation contract (a mistyped scalar is named with
## `InvalidField(name, expected)` and the parse continues), a `Str` cursor over
## the raw query.
##
## Percent-decoding is done ONLY on demanded values (`parse_str` decodes the
## parked value; an undemanded pair is skipped without decoding), which is the
## on-demand win over the old `parse_pairs`-decodes-everything factories.
##
## Values arrive as text, so a query record's fields are `Str` or `I64`
## (`parse_i64`); optionality is `Try(_, [Missing])` and the app supplies the
## default at the use site. A present-but-malformed integer REJECTS
## (`InvalidField`) rather than silently defaulting — an explicit wrong input
## is an error, and a silent default would blur the canonical cache key.
QueryDec := [Default].{
    rename_field : QueryDec, Str -> Str
    rename_field = |_, name| name

    ## The parked value as text, percent-decoded on demand. Never a type error
    ## (any string is a valid Str), so this never accumulates.
    parse_str : QueryDec, QueryState -> Try({ value : Str, rest : QueryState }, [InvalidJson(Str), ..])
    parse_str = |_, state| Ok({ value: percent_decode(state.value), rest: state })

    ## The parked value as I64. A non-integer records `InvalidField` and
    ## fabricates 0 so later fields still parse.
    parse_i64 : QueryDec, QueryState -> Try({ value : I64, rest : QueryState }, [InvalidJson(Str), ..])
    parse_i64 = |_, state| {
        decoded = percent_decode(state.value)
        match I64.from_str(decoded) {
            Ok(n) => Ok({ value: n, rest: state })
            Err(_) => {
                noted = QueryState.{ remaining: state.remaining, value: state.value, field: state.field, errors: state.errors.append(InvalidField(state.field, "an integer")) }
                Ok({ value: 0, rest: noted })
            }
        }
    }

    ## A query has no opening delimiter, so the walk is always Uncounted.
    parse_record_start : QueryDec, QueryState -> Try([Counted({ len : U64, rest : QueryState }), Uncounted(QueryState)], [InvalidJson(Str), ..])
    parse_record_start = |_, state| Ok(Uncounted(state))

    ## Take the next `name=value` pair, name it to the driver, and park the raw
    ## (undecoded) value for whichever scalar parser runs next.
    parse_record_field : QueryDec,
    Encoding.FieldName.FieldNames(_shape),
    QueryState -> Try(
        [
            Field({ field : Encoding.FieldName(_shape), rest : QueryState }),
            TryField({ name : Str, rest : QueryState }),
            TryFieldCaseless({ name : Str, rest : QueryState }),
            Continue(QueryState),
            Done(QueryState),
        ],
        [InvalidJson(Str), ..],
    )
    parse_record_field = |_, _fields, state| {
        if state.remaining.is_empty() {
            Ok(Done(state))
        } else {
            seg = take_segment(state.remaining)
            pair = split_pair(seg.head)
            Ok(TryField({ name: percent_decode(pair.name), rest: QueryState.{ remaining: seg.tail, value: pair.value, field: percent_decode(pair.name), errors: state.errors } }))
        }
    }

    ## Each pair carries its own `&`, already consumed by `take_segment`.
    parse_record_after_field : QueryDec, QueryState -> Try([Continue(QueryState), Done(QueryState)], [InvalidJson(Str), ..])
    parse_record_after_field = |_, state| Ok(Continue(state))

    ## An undemanded pair: its value was already lifted out of `remaining`.
    skip_record_field : QueryDec, QueryState -> Try(QueryState, [InvalidJson(Str), ..])
    skip_record_field = |_, state| Ok(state)

    invalid_value : QueryDec, QueryState -> [InvalidJson(Str), ..]
    invalid_value = |_, _state| InvalidJson("invalid query value")

    ## Decode `raw` into the caller's inferred/annotated query record, returning
    ## the accumulated field mismatches alongside.
    parse : Str -> Try({ value : a, errors : List([InvalidField(Str, Str), InvalidJson(Str), MissingRequiredField(Str)]) }, [InvalidJson(Str), MissingRequiredField(Str), ..errs])
        where [
            a.parser_for : QueryDec -> (QueryState -> Try({ value : a, rest : QueryState }, [InvalidJson(Str), MissingRequiredField(Str), ..errs])),
        ]
    parse = |raw| {
        Shape : a
        parse_shape = Shape.parser_for(QueryDec.Default)
        parsed = parse_shape(QueryState.{ remaining: raw, value: "", field: "", errors: [] })?
        Ok({ value: parsed.value, errors: parsed.rest.errors })
    }
}

QueryState := { remaining : Str, value : Str, field : Str, errors : List([InvalidField(Str, Str), InvalidJson(Str), MissingRequiredField(Str)]) }

## Internal: split "a=1&b=2" into the first segment and the rest.
take_segment : Str -> { head : Str, tail : Str }
take_segment = |raw| match Str.split_first(raw, "&") {
    Ok({ before, after }) => { head: before, tail: after }
    Err(_) => { head: raw, tail: "" }
}

## Internal: split one "name=value" segment. A bare "flag" has an empty value.
split_pair : Str -> { name : Str, value : Str }
split_pair = |segment| match Str.split_first(segment, "=") {
    Ok({ before, after }) => { name: before, value: after }
    Err(_) => { name: segment, value: "" }
}

## Internal: percent-decode, treating '+' as space; invalid escapes pass
## through literally.
percent_decode : Str -> Str
percent_decode = |text| {
    state = text.to_utf8().fold({ out: [], mode: Plain }, |acc, byte|
        match acc.mode {
            Plain =>
                if byte == 37 { { out: acc.out, mode: AfterPercent } }
                else if byte == 43 { { out: acc.out.append(32), mode: Plain } }
                else { { out: acc.out.append(byte), mode: Plain } }
            AfterPercent => match hex_value(byte) {
                Ok(high) => { out: acc.out, mode: AfterHex(high) }
                Err(_) => { out: acc.out.append(37).append(byte), mode: Plain }
            }
            AfterHex(high) => match hex_value(byte) {
                Ok(low) => { out: acc.out.append(high * 16 + low), mode: Plain }
                Err(_) => { out: acc.out.append(37).append(byte), mode: Plain }
            }
        })
    finished = match state.mode {
        Plain => state.out
        AfterPercent => state.out.append(37)
        AfterHex(_) => state.out.append(37)
    }
    Str.from_utf8_lossy(finished)
}

## Internal: value of one hex digit byte.
hex_value : U8 -> Try(U8, [NotHex])
hex_value = |byte|
    if byte >= 48 and byte <= 57 { Ok(byte - 48) }
    else if byte >= 97 and byte <= 102 { Ok(byte - 87) }
    else if byte >= 65 and byte <= 70 { Ok(byte - 55) }
    else { Err(NotHex) }

## a present string field decodes, percent-decoded
expect {
    decoded : Try({ value : { tag : Str }, errors : _ }, _)
    decoded = QueryDec.parse("tag=two%20words")
    match decoded {
        Ok(r) => r.value.tag == "two words" and r.errors.is_empty()
        Err(_) => False
    }
}

## an integer field decodes, and an undemanded pair is skipped without decoding
expect {
    decoded : Try({ value : { limit : I64 }, errors : _ }, _)
    decoded = QueryDec.parse("utm=junk&limit=25&other=x")
    match decoded {
        Ok(r) => r.value.limit == 25
        Err(_) => False
    }
}

## an absent optional field is Err(Missing)
expect {
    decoded : Try({ value : { limit : Try(I64, _) }, errors : _ }, _)
    decoded = QueryDec.parse("offset=3")
    match decoded {
        Ok(r) => r.value.limit == Err(Missing)
        Err(_) => False
    }
}

## a malformed integer is named and accumulated (no silent default)
expect {
    decoded : Try({ value : { limit : I64 }, errors : _ }, _)
    decoded = QueryDec.parse("limit=abc")
    match decoded {
        Ok(r) => r.errors == [InvalidField("limit", "an integer")]
        Err(_) => False
    }
}

## two malformed integers both accumulate
expect {
    decoded : Try({ value : { limit : I64, offset : I64 }, errors : _ }, _)
    decoded = QueryDec.parse("limit=x&offset=y")
    match decoded {
        Ok(r) => r.errors.len() == 2
        Err(_) => False
    }
}
