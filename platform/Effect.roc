import Host

## Host-executed effects and the effectful response shape.
##
## A pure handler returns a `Response` carrying a list of `Effect`s. The host
## runs each effect after the handler returns: `Must` effects are critical and
## carry a boxed `err_handler` the host invokes (with a classified `Err`) when
## the effect fails; `Opt` effects are fire-and-forget.
Effect := [
    Must(Effect.Tag, Box(Effect.Err -> Effect.Reply)),
    Opt(Effect.Tag),
].{
    ## The closed, platform-owned vocabulary of write effects. Reads are
    ## coeffects, resolved before the handler, and never appear here.
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

    ## The classified failure the host hands a `Must` effect's `err_handler`.
    ## Known driver codes map to the typed variants; anything else degrades to
    ## `Other`.
    Err : [
        UniqueViolation(Str),
        ForeignKeyViolation(Str),
        Busy,
        HttpFailed(Str),
        Other(Str),
    ]

    ## The effect-free response an `err_handler` returns. It cannot carry more
    ## effects, which keeps error paths from triggering further I/O.
    Reply : { status : U16, headers : List({ name : Str, value : Str }), body : List(U8) }

    ## The platform's effectful response record: an HTTP response plus the list
    ## of effects the host runs after the handler returns.
    Response : {
        status : U16,
        headers : List({ name : Str, value : Str }),
        body : List(U8),
        effects : List(Effect),
    }

    ## Build a critical effect. The `err_handler` is boxed here because an
    ## unboxed lambda glues to an uninvocable pointer host-side.
    must : Effect.Tag, (Effect.Err -> Effect.Reply) -> Effect
    must = |tag, err_handler| Effect.Must(tag, Box.box(err_handler))

    ## Build a fire-and-forget effect.
    opt : Effect.Tag -> Effect
    opt = |tag| Effect.Opt(tag)

    ## A DbWrite tag: run `sql` (with `params`) against the `db` file.
    db_write : { db : Str, sql : Str, params : List(Host.SqlValue) } -> Effect.Tag
    db_write = |request| DbWrite(request)

    ## An HttpSend tag: send an outbound request.
    http_send : {
        method : Str,
        url : Str,
        headers : List({ name : Str, value : Str }),
        body : List(U8),
        timeout_ms : U64,
    } -> Effect.Tag
    http_send = |request| HttpSend(request)

    ## A Log tag: write `message` through the host logger.
    log : Str -> Effect.Tag
    log = |message| Log(message)
}

# --- Tag builders ---

expect Effect.log("hi") == Log("hi")

expect Effect.db_write({ db: "app.db", sql: "INSERT INTO t VALUES (?)", params: [Host.SqlValue.Integer(1)] }) == DbWrite({ db: "app.db", sql: "INSERT INTO t VALUES (?)", params: [Host.SqlValue.Integer(1)] })

# --- opt: fire-and-forget effect ---
# Effect carries a Box(fn) in its Must variant, so the union has no `==`;
# destructure and compare the Tag (which does support `==`).

expect {
    match Effect.opt(Effect.log("done")) {
        Effect.Opt(tag) => tag == Log("done")
        Effect.Must(_, _) => False
    }
}

# --- must: build, unbox the err_handler, and invoke it ---

expect {
    effect = Effect.must(Effect.db_write({ db: "app.db", sql: "INSERT INTO users VALUES (?)", params: [] }), |err|
        match err {
            UniqueViolation(_) => { status: 409, headers: [], body: "user exists".to_utf8() }
            _ => { status: 500, headers: [], body: "internal error".to_utf8() }
        })
    match effect {
        Effect.Must(_tag, boxed) => {
            handle = Box.unbox(boxed)
            hit = handle(UniqueViolation("users.email"))
            miss = handle(Busy)
            hit.status == 409 and miss.status == 500
        }
        Effect.Opt(_) => False
    }
}

# --- Response: an effectful response carrying a couple of effects ---

expect {
    response : Effect.Response
    response = {
        status: 201,
        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
        body: "created".to_utf8(),
        effects: [
            Effect.must(Effect.db_write({ db: "app.db", sql: "INSERT INTO users VALUES (?)", params: [] }), |err|
                match err {
                    UniqueViolation(_) => { status: 409, headers: [], body: [] }
                    _ => { status: 500, headers: [], body: [] }
                }),
            Effect.opt(Effect.log("created user")),
        ],
    }
    response.status == 201 and response.effects.len() == 2
}
