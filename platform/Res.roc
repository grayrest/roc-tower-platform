## Response construction helpers.
Res := [].{
    ## A SQLite value for effect-carried DbWrite statement parameters. Mirrors
    ## `Host.SqlValue` structurally so the effects list matches the platform's
    ## FFI response type without importing a nominal alias across the boundary.
    ## The closed set of host-executed write effects, matching `Effect.Tag`. The
    ## effects list crosses the FFI as an inline-structural union (see main.roc
    ## `requires`), so it is written structurally here too — a nominal
    ## `List(Effect)` would not unify with that inline type.
    Tag : [
        DbWrite({ db : Str, sql : Str, params : List(Host.SqlValue) }),
        HttpSend({
            method : Str,
            url : Str,
            headers : List({ name : Str, value : Str }),
            body : List(U8),
            timeout_ms : U64,
        }),
        Log(Str),
    ]

    ## The classified failure the host hands a `Must` effect's boxed
    ## `err_handler`. Matches `Effect.Err`.
    Err : [
        UniqueViolation(Str),
        ForeignKeyViolation(Str),
        Busy,
        HttpFailed(Str),
        Other(Str),
    ]

    ## The effect-free response an `err_handler` returns.
    Reply : { status : U16, headers : List({ name : Str, value : Str }), body : List(U8) }

    ## A host-executed effect. `Must` carries a boxed `err_handler` the host
    ## invokes (with a classified `Err`) if the effect fails; `Opt` is
    ## fire-and-forget. Written structurally to unify with the FFI response.
    Effect : [
        Must(Res.Tag, Box(Res.Err -> Res.Reply)),
        Opt(Res.Tag),
    ]

    ## The platform's effectful response record: an HTTP response plus the list
    ## of effects the host runs after the handler returns.
    Response : {
        status : U16,
        headers : List({ name : Str, value : Str }),
        body : List(U8),
        effects : List(Res.Effect),
    }

    ## A plain-text response.
    text : U16, Str -> Res.Response
    text = |status, message| {
        status: status,
        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
        body: message.to_utf8(),
        effects: [],
    }

    ## An HTML response.
    html : U16, Str -> Res.Response
    html = |status, message| {
        status: status,
        headers: [{ name: "content-type", value: "text/html; charset=utf-8" }],
        body: message.to_utf8(),
        effects: [],
    }

    ## A JSON response; the value is encoded with the builtin Json module.
    ## `Json.to_utf8` returns the encoder's bytes directly — no `from_utf8_lossy`
    ## re-validation of already-valid UTF-8, and no Str→bytes round-trip.
    json = |status, value| {
        status: status,
        headers: [{ name: "content-type", value: "application/json" }],
        body: Json.to_utf8(value),
        effects: [],
    }

    ## A response with an explicit content type and raw body bytes.
    bytes : U16, Str, List(U8) -> Res.Response
    bytes = |status, content_type, body| {
        status: status,
        headers: [{ name: "content-type", value: content_type }],
        body: body,
        effects: [],
    }

    ## An empty 204 No Content response.
    no_content : () -> Res.Response
    no_content = || { status: 204, headers: [], body: [], effects: [] }

    ## A redirect (use 301, 302, 303, 307, or 308).
    redirect : U16, Str -> Res.Response
    redirect = |status, location| {
        status: status,
        headers: [{ name: "location", value: location }],
        body: [],
        effects: [],
    }

    ## Add a header to an existing response.
    with_header : Res.Response, Str, Str -> Res.Response
    with_header = |response, name, value| {
        status: response.status,
        headers: response.headers.append({ name: name, value: value }),
        body: response.body,
        effects: response.effects,
    }

    ## The default failure handler used by `Res.fx` / `.fx`: HTTP 500.
    default_err : Res.Err -> Res.Reply
    default_err = |_err| { status: 500, headers: [], body: "effect failed".to_utf8() }

    ## Start a chainable effect builder with a critical effect (default 500 on
    ## failure). Add more with `.fx` / `.fx_err` / `.opt`, then finish with a
    ## response terminal (`.text` / `.json` / `.bytes` / ...):
    ##
    ##     Res.fx(DbWrite({ ... })).opt(Log("...")).text(201, "created")
    fx : Res.Tag -> Res.Fx
    fx = |tag| Res.Fx.{ effects: [Must(tag, Box.box(Res.default_err))] }

    ## Start a chainable effect builder with a critical effect and a custom
    ## failure handler:
    ##
    ##     Res.fx_err(DbWrite({ ... }), |err| ...).json(201, body)
    fx_err : Res.Tag, (Res.Err -> Res.Reply) -> Res.Fx
    fx_err = |tag, err_handler| Res.Fx.{ effects: [Must(tag, Box.box(err_handler))] }

    ## Start a chainable effect builder with a fire-and-forget effect (no
    ## err_handler; failures are logged only). Use for responses that carry
    ## only optional effects:
    ##
    ##     Res.fx_opt(Log("...")).opt(DbWrite({ ... })).text(200, "ok")
    fx_opt : Res.Tag -> Res.Fx
    fx_opt = |tag| Res.Fx.{ effects: [Opt(tag)] }

    ## A chainable builder that accumulates effects and folds them into the
    ## response produced by a terminal method. Built via `Res.fx` / `Res.fx_err`.
    Fx := { effects : List(Res.Effect) }.{
        ## Add a critical effect (default 500 on failure).
        fx : Res.Fx, Res.Tag -> Res.Fx
        fx = |builder, tag| Res.Fx.{ effects: builder.effects.append(Must(tag, Box.box(Res.default_err))) }

        ## Add a critical effect with a custom failure handler.
        fx_err : Res.Fx, Res.Tag, (Res.Err -> Res.Reply) -> Res.Fx
        fx_err = |builder, tag, err_handler| Res.Fx.{ effects: builder.effects.append(Must(tag, Box.box(err_handler))) }

        ## Add a fire-and-forget effect.
        opt : Res.Fx, Res.Tag -> Res.Fx
        opt = |builder, tag| Res.Fx.{ effects: builder.effects.append(Opt(tag)) }

        ## Terminal: a plain-text response carrying the accumulated effects.
        text : Res.Fx, U16, Str -> Res.Response
        text = |builder, status, message| {
            status: status,
            headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
            body: message.to_utf8(),
            effects: builder.effects,
        }

        ## Terminal: an HTML response carrying the accumulated effects.
        html : Res.Fx, U16, Str -> Res.Response
        html = |builder, status, message| {
            status: status,
            headers: [{ name: "content-type", value: "text/html; charset=utf-8" }],
            body: message.to_utf8(),
            effects: builder.effects,
        }

        ## Terminal: a JSON response (builtin Json encoding) with the effects.
        json = |builder, status, value| {
            status: status,
            headers: [{ name: "content-type", value: "application/json" }],
            body: Json.to_utf8(value),
            effects: builder.effects,
        }

        ## Terminal: raw body bytes with an explicit content type and the effects.
        bytes : Res.Fx, U16, Str, List(U8) -> Res.Response
        bytes = |builder, status, content_type, body| {
            status: status,
            headers: [{ name: "content-type", value: content_type }],
            body: body,
            effects: builder.effects,
        }

        ## Terminal: 204 No Content carrying the accumulated effects.
        no_content : Res.Fx -> Res.Response
        no_content = |builder| { status: 204, headers: [], body: [], effects: builder.effects }

        ## Terminal: a redirect carrying the accumulated effects.
        redirect : Res.Fx, U16, Str -> Res.Response
        redirect = |builder, status, location| {
            status: status,
            headers: [{ name: "location", value: location }],
            body: [],
            effects: builder.effects,
        }
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

    ## Adapt a fallible handler whose error is a DOMAIN type into the boxed
    ## `Request => Response` the router registers: run it, pass `Ok` through,
    ## and `render` the `Err` into a response. This lets a handler use `?` over
    ## its own error union — no `from_try` in the body, no per-`?` response
    ## building, and no return-type annotation on the handler (its type is
    ## inferred from the `?` tags and `render`). `Req.route` builds a whole
    ## route-table entry on top of this; use `Res.mapped` directly only when
    ## assembling the entry record yourself:
    ##
    ##     { method: "POST", path: "/x", handler: Res.mapped(show!, my_render) }
    mapped : (req => Try(Res.Response, err)), (err -> Res.Response) -> Box(req => Res.Response)
    mapped = |handler, render| Box.box(|req| match handler(req) {
        Ok(response) => response
        Err(error) => render(error)
    })
}

import Host

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

# --- chainable effect builder ---

expect {
    response = Res.fx(Log("recorded")).text(201, "ok")
    response.status == 201 and response.effects.len() == 1 and response.body == "ok".to_utf8()
}

expect {
    response =
        Res.fx(DbWrite({ db: "d", sql: "insert", params: [] }))
            .opt(Log("also"))
            .text(200, "done")
    response.status == 200 and response.effects.len() == 2
}

expect {
    response =
        Res.fx_err(
            DbWrite({ db: "d", sql: "insert", params: [] }),
            |_err| { status: 409, headers: [], body: "taken".to_utf8() },
        )
        .json(201, { id: 1.U64 })
    response.status == 201 and response.effects.len() == 1
}

expect {
    response =
        Res.fx_opt(Log("first"))
            .opt(DbWrite({ db: "d", sql: "insert", params: [] }))
            .text(200, "ok")
    response.status == 200 and response.effects.len() == 2 and response.body == "ok".to_utf8()
}
