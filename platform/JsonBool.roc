## A Bool that JSON-encodes as real `true`/`false`. The compiler's derived
## encoder currently renders a `Bool` record field as the STRING `"True"` /
## `"False"` (tag-union stringification) even though `Json.encode_bool`
## exists and DECODE of `true`/`false` into `Bool` works — see
## notes/2026-07-12-compiler-bool-encode-gap.md. This nominal routes encoding
## through `encode_bool`; delete it (and switch fields back to plain `Bool`)
## once the derivation is fixed upstream.
JsonBool := { value : Bool }.{
    ## The `Json.to_str` hook: encode the wrapped Bool as JSON true/false.
    encoder_for = |encoding| |wrapped, state| Json.encode_bool(encoding, wrapped.value, state)

    ## Wrap a Bool for JSON output.
    from : Bool -> JsonBool
    from = |value| JsonBool.{ value: value }

    ## The wrapped Bool, for logic on values already in JSON shape.
    truth : JsonBool -> Bool
    truth = |wrapped| wrapped.value
}

## a True flag encodes as JSON true (not the string "True")
expect Json.to_str({ favorited: JsonBool.from(True) }) == "{\"favorited\":true}"

## a False flag encodes as JSON false
expect Json.to_str({ favorited: JsonBool.from(False) }) == "{\"favorited\":false}"

## truth unwraps what from wrapped
expect JsonBool.truth(JsonBool.from(True)) and !JsonBool.truth(JsonBool.from(False))
