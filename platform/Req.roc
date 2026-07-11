## Request inspection helpers. All fallible helpers return Try; bridge into
## a Response with Res.from_try.
Req := [].{
    ## Look up a path parameter by name.
    param : { params : List({ name : Str, value : Str }), .. }, Str -> Try(Str, [MissingParam])
    param = |req, name| {
        match req.params.find_first(|entry| entry.name == name) {
            Ok(entry) => Ok(entry.value)
            Err(_) => Err(MissingParam)
        }
    }

    ## Look up a request header by name (case-insensitive).
    header : { headers : List({ name : Str, value : Str }), .. }, Str -> Try(Str, [MissingHeader])
    header = |req, name| {
        wanted = Req.ascii_lower(name)
        match req.headers.find_first(|entry| Req.ascii_lower(entry.name) == wanted) {
            Ok(entry) => Ok(entry.value)
            Err(_) => Err(MissingHeader)
        }
    }

    ## All query parameters, percent-decoded, in order of appearance.
    query_params : { query : Str, .. } -> List({ name : Str, value : Str })
    query_params = |req| Req.parse_pairs(req.query)

    ## Look up a single query parameter by name (first occurrence wins).
    query : { query : Str, .. }, Str -> Try(Str, [MissingQueryParam])
    query = |req, name| {
        match Req.parse_pairs(req.query).find_first(|entry| entry.name == name) {
            Ok(entry) => Ok(entry.value)
            Err(_) => Err(MissingQueryParam)
        }
    }

    ## The request body as UTF-8 text.
    body_str : { body : List(U8), .. } -> Try(Str, [InvalidUtf8])
    body_str = |req| {
        match Str.from_utf8(req.body) {
            Ok(text) => Ok(text)
            Err(_) => Err(InvalidUtf8)
        }
    }

    ## Decode the request body as JSON into any parseable type.
    json = |req| {
        match Str.from_utf8(req.body) {
            Ok(text) => match Json.parse(text) {
                Ok(value) => Ok(value)
                Err(err) => Err(InvalidJson(err))
            }
            Err(_) => Err(InvalidUtf8)
        }
    }

    ## Parse an application/x-www-form-urlencoded body.
    form : { body : List(U8), .. } -> Try(List({ name : Str, value : Str }), [InvalidUtf8])
    form = |req| {
        match Str.from_utf8(req.body) {
            Ok(text) => Ok(Req.parse_pairs(text))
            Err(_) => Err(InvalidUtf8)
        }
    }

    ## Internal: parse "a=1&b=two%20words" into decoded pairs.
    parse_pairs : Str -> List({ name : Str, value : Str })
    parse_pairs = |raw| {
        raw.split_on("&")
            .map(|segment| Req.split_pair(segment))
            .keep_if(|entry| !(entry.name.is_empty() and entry.value.is_empty()))
    }

    ## Internal: split one "name=value" segment and decode both sides.
    split_pair : Str -> { name : Str, value : Str }
    split_pair = |segment| {
        parts = segment.split_on("=")
        name = match parts.first() {
            Ok(part) => part
            Err(_) => ""
        }
        value = match parts.len() {
            0 => ""
            1 => ""
            _ => Str.join_with(parts.sublist({ start: 1, len: parts.len() - 1 }), "=")
        }
        { name: Req.percent_decode(name), value: Req.percent_decode(value) }
    }

    ## Internal: percent-decode, treating '+' as space. Invalid escapes pass
    ## through literally.
    percent_decode : Str -> Str
    percent_decode = |text| {
        state = text.to_utf8().fold({ out: [], mode: Plain }, |acc, byte| {
            match acc.mode {
                Plain => {
                    if byte == 37 {
                        # '%'
                        { out: acc.out, mode: AfterPercent }
                    } else if byte == 43 {
                        # '+' -> space
                        { out: acc.out.append(32), mode: Plain }
                    } else {
                        { out: acc.out.append(byte), mode: Plain }
                    }
                }
                AfterPercent => {
                    match Req.hex_value(byte) {
                        Ok(high) => { out: acc.out, mode: AfterHex(high) }
                        Err(_) => { out: acc.out.append(37).append(byte), mode: Plain }
                    }
                }
                AfterHex(high) => {
                    match Req.hex_value(byte) {
                        Ok(low) => { out: acc.out.append(high * 16 + low), mode: Plain }
                        Err(_) => { out: acc.out.append(37).append(byte), mode: Plain }
                    }
                }
            }
        })
        finished = match state.mode {
            Plain => state.out
            AfterPercent => state.out.append(37)
            AfterHex(_) => state.out.append(37)
        }
        Str.from_utf8_lossy(finished)
    }

    ## Internal: the value of one hex digit byte.
    hex_value : U8 -> Try(U8, [NotHex])
    hex_value = |byte| {
        if byte >= 48 and byte <= 57 {
            Ok(byte - 48)
        } else if byte >= 97 and byte <= 102 {
            Ok(byte - 87)
        } else if byte >= 65 and byte <= 70 {
            Ok(byte - 55)
        } else {
            Err(NotHex)
        }
    }

    ## Internal: ASCII-lowercase a string (header comparison).
    ascii_lower : Str -> Str
    ascii_lower = |text| {
        lowered = text.to_utf8().map(|byte| {
            if byte >= 65 and byte <= 90 {
                byte + 32
            } else {
                byte
            }
        })
        Str.from_utf8_lossy(lowered)
    }
}

expect Req.param({ params: [{ name: "id", value: "42" }] }, "id") == Ok("42")

expect Req.param({ params: [] }, "id") == Err(MissingParam)

expect Req.header({ headers: [{ name: "Content-Type", value: "text/plain" }] }, "content-type") == Ok("text/plain")

expect Req.header({ headers: [{ name: "x-a", value: "1" }] }, "X-A") == Ok("1")

expect Req.header({ headers: [] }, "accept") == Err(MissingHeader)

expect Req.query_params({ query: "a=1&b=two%20words" }) == [{ name: "a", value: "1" }, { name: "b", value: "two words" }]

expect Req.query_params({ query: "" }) == []

expect Req.query_params({ query: "flag" }) == [{ name: "flag", value: "" }]

expect Req.query({ query: "a=1&a=2" }, "a") == Ok("1")

expect Req.query({ query: "a=1" }, "b") == Err(MissingQueryParam)

# '+' decodes as space; '=' inside a value survives.
expect Req.query_params({ query: "q=a+b&eq=x%3Dy=z" }) == [{ name: "q", value: "a b" }, { name: "eq", value: "x=y=z" }]

# Invalid escapes pass through literally.
expect Req.percent_decode("100%") == "100%"

expect Req.percent_decode("%zz") == "%zz"

expect Req.percent_decode("%2F%2f") == "//"

expect Req.body_str({ body: [104, 105] }) == Ok("hi")

expect Req.body_str({ body: [255, 254] }) == Err(InvalidUtf8)

expect Req.form({ body: "a=1&b=%2B".to_utf8() }) == Ok([{ name: "a", value: "1" }, { name: "b", value: "+" }])

expect {
    decoded : Try({ id : U64 }, _)
    decoded = Req.json({ body: "{\"id\":7}".to_utf8() })
    decoded == Ok({ id: 7 })
}
