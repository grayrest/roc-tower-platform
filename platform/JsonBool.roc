## A Bool that JSON-encodes as real `true`/`false`. The compiler's derived
## encoder currently renders a `Bool` record field as the STRING `"True"` /
## `"False"` (tag-union stringification) even though `Json.encode_bool`
## exists and DECODE of `true`/`false` into `Bool` works — see
## notes/2026-07-12-compiler-bool-encode-gap.md. This nominal routes encoding
## through `encode_bool`; delete it (and switch fields back to plain `Bool`)
## once the derivation is fixed upstream.
import JsonEnc

JsonBool := { value : Bool }.{
    ## The encode hook, typed to the PLATFORM's `JsonEnc` format rather than the
    ## builtin's. A nominal can serve exactly one encoding: a generic
    ## `encoder_for` spanning two formats panics the compiler (see
    ## examples/repro/GenericEncoderTwoFormats.roc), so this deliberately gives
    ## up `Json.to_str` on `JsonBool` — which is now a clean compile error.
    encoder_for = |_encoding| |wrapped, state| JsonEnc.encode_bool(wrapped.value, state)

    ## Wrap a Bool for JSON output.
    from : Bool -> JsonBool
    from = |value| JsonBool.{ value: value }

    ## The wrapped Bool, for logic on values already in JSON shape.
    truth : JsonBool -> Bool
    truth = |wrapped| wrapped.value
}

## a True flag encodes as JSON true (not the string "True")
expect JsonEnc.to_str({ favorited: JsonBool.from(True) }) == "{\"favorited\":true}"

## a False flag encodes as JSON false
expect JsonEnc.to_str({ favorited: JsonBool.from(False) }) == "{\"favorited\":false}"

## truth unwraps what from wrapped
expect JsonBool.truth(JsonBool.from(True)) and !JsonBool.truth(JsonBool.from(False))
