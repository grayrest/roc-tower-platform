# Minimal repro for a roc compiler PANIC (reproduces on release-fast-917abf7f,
# 2026-08-04):
#
#     thread N panic: one static-dispatch edge selected two different method bindings
#
# A nominal whose `encoder_for` is GENERIC over the encoding — annotated with a
# `where [enc.encode_str : ...]` constraint rather than naming a concrete
# format — works fine when exactly one format dispatches it (see the sibling
# single-format version: it checks, and its expect runs). Add a SECOND format
# that encodes the same nominal and the compiler panics during checking.
#
#   roc check examples/repro/GenericEncoderTwoFormats.roc   # panic
#
# Deleting either `encode_b`/`B` or the generic `where` clause (typing
# `encoder_for` to one concrete format, as upstream's
# test/cli/EncoderForStructuralRecord.roc does) avoids it.
#
# Impact here: a platform nominal like `NullableStr` cannot supply one generic
# `encoder_for` serving both the builtin `JsonEncoding` and a vendored
# platform format — so the vendored format must OWN those nominals outright
# (concrete typing), which is a clean compile error if misused rather than a
# panic. See plans/2026-08-03-encoders-decoders.md phase 1.
GenericEncoderTwoFormats :: [].{}

A := [Default].{
    rename_field : A, Str -> Str
    rename_field = |_, name| name
    encode_str : Str, U64 -> Try(U64, [])
    encode_str = |value, state| Ok(state + Str.count_utf8_bytes(value))
}

# ONE format only, and a single-method where clause.
Wrapper := { raw : Str }.{
    encoder_for : enc -> (Wrapper, U64 -> Try(U64, []))
        where [enc.encode_str : Str, U64 -> Try(U64, [])]
    encoder_for = |_encoding| {
        Enc : enc
        |wrapped, state| Enc.encode_str(wrapped.raw, state)
    }
}

B := [Default].{
    rename_field : B, Str -> Str
    rename_field = |_, name| name
    encode_str : Str, U64 -> Try(U64, [])
    encode_str = |_value, state| Ok(state + 100)
}

encode_b : value -> Try(U64, []) where [value.encoder_for : B -> (value, U64 -> Try(U64, []))]
encode_b = |value| {
    Shape : value
    e = Shape.encoder_for(B.Default)
    e(value, 0)
}

expect encode_b(Wrapper.{ raw: "abc" }) == Ok(100)

encode_a : value -> Try(U64, []) where [value.encoder_for : A -> (value, U64 -> Try(U64, []))]
encode_a = |value| {
    Shape : value
    e = Shape.encoder_for(A.Default)
    e(value, 0)
}

expect encode_a(Wrapper.{ raw: "abc" }) == Ok(3)
