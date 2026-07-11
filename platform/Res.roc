## Response construction helpers.
Res := [].{
    ## The platform's response record.
    Response : { status : U16, headers : List({ name : Str, value : Str }), body : List(U8) }

    ## A plain-text response.
    text : U16, Str -> Res.Response
    text = |status, message| {
        status: status,
        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
        body: message.to_utf8(),
    }

    ## An HTML response.
    html : U16, Str -> Res.Response
    html = |status, message| {
        status: status,
        headers: [{ name: "content-type", value: "text/html; charset=utf-8" }],
        body: message.to_utf8(),
    }

    ## A JSON response; the value is encoded with the builtin Json module.
    json = |status, value| {
        status: status,
        headers: [{ name: "content-type", value: "application/json" }],
        body: Json.to_str(value).to_utf8(),
    }

    ## A response with an explicit content type and raw body bytes.
    bytes : U16, Str, List(U8) -> Res.Response
    bytes = |status, content_type, body| {
        status: status,
        headers: [{ name: "content-type", value: content_type }],
        body: body,
    }

    ## An empty 204 No Content response.
    no_content : () -> Res.Response
    no_content = || { status: 204, headers: [], body: [] }

    ## A redirect (use 301, 302, 303, 307, or 308).
    redirect : U16, Str -> Res.Response
    redirect = |status, location| {
        status: status,
        headers: [{ name: "location", value: location }],
        body: [],
    }

    ## Add a header to an existing response.
    with_header : Res.Response, Str, Str -> Res.Response
    with_header = |response, name, value| {
        status: response.status,
        headers: response.headers.append({ name: name, value: value }),
        body: response.body,
    }

    ## Collapse a Try whose both sides are responses. Lets handlers delegate
    ## to an inner `Request => Try(Response, Response)` function where `?`
    ## works, then merge:
    ##
    ##     handler! = |req| Res.from_try(attempt!(req))
    from_try : Try(Res.Response, Res.Response) -> Res.Response
    from_try = |result| match result {
        Ok(response) => response
        Err(response) => response
    }
}

expect {
    response = Res.text(200, "hi")
    response.status == 200 and response.body == [104, 105]
}

expect {
    response = Res.text(404, "missing")
    match response.headers.first() {
        Ok(header) => header.value == "text/plain; charset=utf-8"
        Err(_) => False
    }
}

expect {
    value : { id : U64, name : Str }
    value = { id: 7, name: "x" }
    response = Res.json(201, value)
    Str.from_utf8_lossy(response.body) == "{\"id\":7,\"name\":\"x\"}"
}

expect {
    response = Res.redirect(302, "/login")
    match response.headers.first() {
        Ok(header) => header.name == "location" and header.value == "/login"
        Err(_) => False
    }
}

expect {
    response = Res.no_content()
    response.status == 204 and response.body.is_empty()
}

expect {
    response = Res.with_header(Res.text(200, "x"), "x-extra", "1")
    match response.headers.get(1) {
        Ok(header) => header.name == "x-extra"
        Err(_) => False
    }
}

expect Res.from_try(Ok(Res.text(200, "a"))).status == 200

expect Res.from_try(Err(Res.text(500, "b"))).status == 500
