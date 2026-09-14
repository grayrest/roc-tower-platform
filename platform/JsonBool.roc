## A Bool that JSON-encodes as real `true`/`false`. The compiler's derived
## encoder currently renders a `Bool` record field as the STRING `"True"` /
## `"False"` (tag-union stringification) even though `Json.encode_bool`
## exists and DECODE of `true`/`false` into `Bool` works — see
## notes/2026-07-12-compiler-bool-encode-gap.md. This nominal routes encoding
## through `encode_bool`; delete it (and switch fields back to plain `Bool`)
## once the derivation is fixed upstream.
import JsonEnc

JsonBool := { value : Bool }.{
    ## The encode hook, generic over the format: `JsonEnc` writes it as JSON
    ## and `HashFormat` hashes it. (A generic `encoder_for` spanning two formats
    ## used to panic the compiler — examples/repro/GenericEncoderTwoFormats.roc;
    ## it no longer does on roc 10e922df.)
    encoder_for : enc -> (JsonBool, state -> Try(state, err))
        where [enc.encode_bool : Bool, state -> Try(state, err)]
    encoder_for = |_encoding| {
        Enc : enc
        |wrapped, state| Enc.encode_bool(wrapped.value, state)
    }

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
