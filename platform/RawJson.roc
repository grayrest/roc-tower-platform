## Preformed JSON text — a document assembled in SQL (`json_object` /
## `json_group_array`) and spliced verbatim into a response body, never
## re-encoded by `Json.to_str`. Produced by `@sql json` generated readers and
## `Sql.query_json!`, consumed by `Res.json_raw`.
##
## The point is to skip the row → nested-record → encode pipeline entirely:
## SQLite emits the response bytes, the host copies them once into the
## response. There is deliberately NO `encoder_for` hook — a `RawJson` is
## never a field inside another `Json.to_str` value; it IS the body.
RawJson := { text : Str }.{
    ## Wrap SQL-assembled JSON text.
    from : Str -> RawJson
    from = |text| RawJson.{ text: text }

    ## The raw JSON text (for building an envelope around it).
    to_str : RawJson -> Str
    to_str = |doc| doc.text

    ## The raw JSON bytes (for a response body).
    to_utf8 : RawJson -> List(U8)
    to_utf8 = |doc| doc.text.to_utf8()
}

## from/to_str round-trips the text
expect RawJson.to_str(RawJson.from("{\"a\":1}")) == "{\"a\":1}"

## to_utf8 yields the underlying bytes
expect RawJson.to_utf8(RawJson.from("[]")) == "[]".to_utf8()
