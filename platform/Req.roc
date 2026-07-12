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

    ## The raw request body bytes. Extraction never fails.
    Body := [Body(List(U8))].{
        from_request : { body : List(U8), .. } -> Try(Body, resp)
        from_request = |req| Ok(Body.Body(req.body))

        value : Body -> List(U8)
        value = |wrapped| match wrapped { Body.Body(bytes) => bytes }
    }

    ## The request body as UTF-8 text. Rejects with 400 on invalid UTF-8.
    BodyStr := [BodyStr(Str)].{
        from_request = |req| {
            match Req.body_str(req) {
                Ok(text) => Ok(BodyStr.BodyStr(text))
                Err(_) => Err(Req.reject(400, "request body is not valid UTF-8"))
            }
        }

        value : BodyStr -> Str
        value = |wrapped| match wrapped { BodyStr.BodyStr(text) => text }
    }

    ## The request body decoded as JSON into any parseable type. Rejects
    ## with 400 when the body is not valid JSON for that type.
    JsonBody(a) := [JsonBody(a)].{
        from_request = |req| {
            match Req.json(req) {
                Ok(decoded) => Ok(JsonBody.JsonBody(decoded))
                Err(_) => Err(Req.reject(400, "request body is not valid JSON for the expected shape"))
            }
        }

        value : JsonBody(a) -> a
        value = |wrapped| match wrapped { JsonBody.JsonBody(decoded) => decoded }
    }

    ## The request body parsed as application/x-www-form-urlencoded pairs.
    ## Rejects with 400 on invalid UTF-8.
    FormBody := [FormBody(List({ name : Str, value : Str }))].{
        from_request = |req| {
            match Req.form(req) {
                Ok(fields) => Ok(FormBody.FormBody(fields))
                Err(_) => Err(Req.reject(400, "request body is not a valid urlencoded form"))
            }
        }

        value : FormBody -> List({ name : Str, value : Str })
        value = |wrapped| match wrapped { FormBody.FormBody(fields) => fields }

        get : FormBody, Str -> Try(Str, [MissingField])
        get = |wrapped, name| {
            match FormBody.value(wrapped).find_first(|entry| entry.name == name) {
                Ok(entry) => Ok(entry.value)
                Err(_) => Err(MissingField)
            }
        }
    }

    ## All query parameters, percent-decoded. Extraction never fails.
    QueryParams := [QueryParams(List({ name : Str, value : Str }))].{
        from_request : { query : Str, .. } -> Try(QueryParams, resp)
        from_request = |req| Ok(QueryParams.QueryParams(Req.query_params(req)))

        value : QueryParams -> List({ name : Str, value : Str })
        value = |wrapped| match wrapped { QueryParams.QueryParams(pairs) => pairs }

        get : QueryParams, Str -> Try(Str, [MissingQueryParam])
        get = |wrapped, name| {
            match QueryParams.value(wrapped).find_first(|entry| entry.name == name) {
                Ok(entry) => Ok(entry.value)
                Err(_) => Err(MissingQueryParam)
            }
        }
    }

    ## All request headers. Extraction never fails.
    Headers := [Headers(List({ name : Str, value : Str }))].{
        from_request : { headers : List({ name : Str, value : Str }), .. } -> Try(Headers, resp)
        from_request = |req| Ok(Headers.Headers(req.headers))

        value : Headers -> List({ name : Str, value : Str })
        value = |wrapped| match wrapped { Headers.Headers(pairs) => pairs }

        get : Headers, Str -> Try(Str, [MissingHeader])
        get = |wrapped, name| Req.header({ headers: Headers.value(wrapped) }, name)
    }

    ## The app's shared model, unboxed. Extraction never fails.
    Model(m) := [Model(m)].{
        from_request = |req| Ok(Model.Model(Box.unbox(req.model)))

        value : Model(m) -> m
        value = |wrapped| match wrapped { Model.Model(model) => model }
    }

    ## Wrap a 1-argument handler into a request handler. The argument type's
    ## from_request method extracts it from the request; an Err rejection is
    ## returned as the response. Arguments extract left to right; the first
    ## rejection wins.
    args1 : (a => resp) -> (req => resp)
        where [a.from_request : req -> Try(a, resp)]
    args1 = |handler| {
        A : a
        |req| match A.from_request(req) {
            Ok(x) => handler(x)
            Err(rejection) => rejection
        }
    }

    ## Wrap a 2-argument handler into a request handler (see args1).
    args2 : (a, b => resp) -> (req => resp)
        where [
            a.from_request : req -> Try(a, resp),
            b.from_request : req -> Try(b, resp),
        ]
    args2 = |handler| {
        A : a
        B : b
        |req| match A.from_request(req) {
            Err(rejection) => rejection
            Ok(x) => match B.from_request(req) {
                Err(rejection) => rejection
                Ok(y) => handler(x, y)
            }
        }
    }

    ## Wrap a 3-argument handler into a request handler (see args1).
    args3 : (a, b, c => resp) -> (req => resp)
        where [
            a.from_request : req -> Try(a, resp),
            b.from_request : req -> Try(b, resp),
            c.from_request : req -> Try(c, resp),
        ]
    args3 = |handler| {
        A : a
        B : b
        C : c
        |req| match A.from_request(req) {
            Err(rejection) => rejection
            Ok(x) => match B.from_request(req) {
                Err(rejection) => rejection
                Ok(y) => match C.from_request(req) {
                    Err(rejection) => rejection
                    Ok(z) => handler(x, y, z)
                }
            }
        }
    }

    ## Wrap a 4-argument handler into a request handler (see args1).
    args4 : (a, b, c, d => resp) -> (req => resp)
        where [
            a.from_request : req -> Try(a, resp),
            b.from_request : req -> Try(b, resp),
            c.from_request : req -> Try(c, resp),
            d.from_request : req -> Try(d, resp),
        ]
    args4 = |handler| {
        A : a
        B : b
        C : c
        D : d
        |req| match A.from_request(req) {
            Err(rejection) => rejection
            Ok(x) => match B.from_request(req) {
                Err(rejection) => rejection
                Ok(y) => match C.from_request(req) {
                    Err(rejection) => rejection
                    Ok(z) => match D.from_request(req) {
                        Err(rejection) => rejection
                        Ok(w) => handler(x, y, z, w)
                    }
                }
            }
        }
    }

    ## Wrap a 5-argument handler into a request handler (see args1).
    args5 : (a, b, c, d, e => resp) -> (req => resp)
        where [
            a.from_request : req -> Try(a, resp),
            b.from_request : req -> Try(b, resp),
            c.from_request : req -> Try(c, resp),
            d.from_request : req -> Try(d, resp),
            e.from_request : req -> Try(e, resp),
        ]
    args5 = |handler| {
        A : a
        B : b
        C : c
        D : d
        E : e
        |req| match A.from_request(req) {
            Err(rejection) => rejection
            Ok(x) => match B.from_request(req) {
                Err(rejection) => rejection
                Ok(y) => match C.from_request(req) {
                    Err(rejection) => rejection
                    Ok(z) => match D.from_request(req) {
                        Err(rejection) => rejection
                        Ok(w) => match E.from_request(req) {
                            Err(rejection) => rejection
                            Ok(v) => handler(x, y, z, w, v)
                        }
                    }
                }
            }
        }
    }

    ## Internal: a plain-text rejection response for extractor failures.
    ## Left unannotated: naming the 4-field effectful Response type would need
    ## to reference a sibling platform module, which annotations can't do. The
    ## empty `effects: []` unifies with whatever the app's Response pins.
    reject = |status, message| {
        status: status,
        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
        body: Str.to_utf8(message),
        effects: [],
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

# --- extractor types (from_request / accessors) ---

expect Req.Body.from_request({ body: [104, 105] }) == Ok(Req.Body.Body([104, 105]))

expect Req.Body.value(Req.Body.Body([1, 2])) == [1, 2]

expect Req.BodyStr.from_request({ body: [104, 105] }) == Ok(Req.BodyStr.BodyStr("hi"))

expect Req.BodyStr.from_request({ body: [255, 254] }) == Err(Req.reject(400, "request body is not valid UTF-8"))

expect {
    extracted : Try(Req.JsonBody({ id : U64 }), _)
    extracted = Req.JsonBody.from_request({ body: "{\"id\":7}".to_utf8() })
    match extracted {
        Ok(wrapped) => wrapped.value() == { id: 7 }
        Err(_) => False
    }
}

expect {
    extracted : Try(Req.JsonBody({ id : U64 }), _)
    extracted = Req.JsonBody.from_request({ body: "nope".to_utf8() })
    extracted == Err(Req.reject(400, "request body is not valid JSON for the expected shape"))
}

expect {
    extracted = Req.FormBody.from_request({ body: "a=1&b=x+y".to_utf8() })
    match extracted {
        Ok(wrapped) => wrapped.get("b") == Ok("x y")
        Err(_) => False
    }
}

expect Req.QueryParams.get(Req.QueryParams.QueryParams([{ name: "q", value: "zed" }]), "q") == Ok("zed")

expect {
    extracted = Req.QueryParams.from_request({ query: "a=1&b=two%20words" })
    match extracted {
        Ok(wrapped) => wrapped.value() == [{ name: "a", value: "1" }, { name: "b", value: "two words" }]
        Err(_) => False
    }
}

expect Req.Headers.get(Req.Headers.Headers([{ name: "X-Custom", value: "abc" }]), "x-custom") == Ok("abc")

expect {
    extracted = Req.Model.from_request({ model: Box.box({ greeting: "hey" }) })
    match extracted {
        Ok(wrapped) => wrapped.value() == { greeting: "hey" }
        Err(_) => False
    }
}
