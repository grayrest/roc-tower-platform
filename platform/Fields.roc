## Compile-time-ish field introspection over a record type: derive the column
## list a `SELECT` should name from the record its rows decode into, with no
## value ever constructed. A format that abuses the parser protocol — its first
## `parse_record_field` reads the demanded `FieldNames` and FAILS with them, so
## the driver hands the names straight back.
##
## This is the on-ramp to a real typed-SQL read (`parser_for!`, phase 8): the
## SAME introspection that names the columns would, with an effectful parser
## derive, also decode the cursor. Until then it derives the column list only.
##
## NOTE (probed, phase 0): `FieldNames.iter` yields names in ALPHABETICAL
## order, not declaration order. Safe when the emitted `SELECT` names columns
## explicitly and the row is read BY NAME; a positional reader paired with this
## must sort its getters to match.
Fields := [].{
    ## The column names of `a`'s record type, in `FieldNames` (alphabetical)
    ## order. `a` is pinned by the caller's annotation on the result the SELECT
    ## feeds — e.g. `cols = Fields.of(phantom)` where `phantom : Meta`.
    of : a -> List(Str)
        where [
            a.parser_for : FieldsEncoding -> (FieldsState -> Try({ value : a, rest : FieldsState }, [CollectedFields(List(Str)), ..errs])),
        ]
    of = |_phantom| {
        Shape : a
        parse_shape = Shape.parser_for(FieldsEncoding.Default)
        match parse_shape(FieldsState.{ collected: [] }) {
            Err(CollectedFields(names)) => names
            _ => []
        }
    }
}

## The introspection format. Every method exists only to satisfy the derived
## parser's shape; the real work is in `parse_record_field`, which never lets
## the driver get past the first field.
FieldsEncoding := [Default].{
    rename_field : FieldsEncoding, Str -> Str
    rename_field = |_, name| name

    parse_record_start : FieldsEncoding, FieldsState -> Try([Counted({ len : U64, rest : FieldsState }), Uncounted(FieldsState)], [CollectedFields(List(Str)), ..])
    parse_record_start = |_, state| Ok(Uncounted(state))

    ## The whole mechanism: read every demanded name off `FieldNames` and FAIL
    ## with the list. The driver propagates the `Err`; `Fields.of` catches it.
    parse_record_field : FieldsEncoding,
    Encoding.FieldName.FieldNames(_shape),
    FieldsState -> Try(
        [
            Field({ field : Encoding.FieldName(_shape), rest : FieldsState }),
            TryField({ name : Str, rest : FieldsState }),
            TryFieldCaseless({ name : Str, rest : FieldsState }),
            Continue(FieldsState),
            Done(FieldsState),
        ],
        [CollectedFields(List(Str)), ..],
    )
    parse_record_field = |_, fields, _state| {
        names = Encoding.FieldName.FieldNames.iter(fields)
            .fold([], |acc, f| acc.append(Encoding.FieldName.name(f)))
        Err(CollectedFields(names))
    }

    parse_record_after_field : FieldsEncoding, FieldsState -> Try([Continue(FieldsState), Done(FieldsState)], [CollectedFields(List(Str)), ..])
    parse_record_after_field = |_, state| Ok(Done(state))

    skip_record_field : FieldsEncoding, FieldsState -> Try(FieldsState, [CollectedFields(List(Str)), ..])
    skip_record_field = |_, state| Ok(state)

    ## Scalars are never reached (the walk fails at the first field), but the
    ## derive needs them present for a record of these field types.
    parse_str : FieldsEncoding, FieldsState -> Try({ value : Str, rest : FieldsState }, [CollectedFields(List(Str)), ..])
    parse_str = |_, state| Ok({ value: "", rest: state })

    parse_i64 : FieldsEncoding, FieldsState -> Try({ value : I64, rest : FieldsState }, [CollectedFields(List(Str)), ..])
    parse_i64 = |_, state| Ok({ value: 0, rest: state })

    invalid_value : FieldsEncoding, FieldsState -> [CollectedFields(List(Str)), ..]
    invalid_value = |_, _state| CollectedFields([])
}

FieldsState := { collected : List(Str) }

## the column list of a two-field record derives (alphabetical order)
expect {
    phantom : { author_id : I64, id : I64 }
    phantom = { author_id: 0, id: 0 }
    Fields.of(phantom) == ["author_id", "id"]
}

## a single-field record derives its one column
expect {
    phantom : { tag : Str }
    phantom = { tag: "" }
    Fields.of(phantom) == ["tag"]
}

## the derived list builds a SELECT column clause — the intended use
expect {
    phantom : { id : I64, author_id : I64 }
    phantom = { id: 0, author_id: 0 }
    clause = Str.join_with(Fields.of(phantom), ", ")
    "SELECT ${clause} FROM articles" == "SELECT author_id, id FROM articles"
}
