## Request inspection: pure helpers for raw handlers, and the built-in
## coeffect resolvers/factories for pure handlers (see `Cofx`).
Req := [].{
    ## Look up a path parameter by name. (The route-facing coeffect factory
    ## is `Req.param(name)`; this is the direct helper for raw handlers and
    ## resolver bodies.)
    get_param : { params : List({ name : Str, value : Str }), .. }, Str -> Try(Str, [MissingParam])
    get_param = |req, name| {
        entry = req.params.find_first(|candidate| candidate.name == name) ? |_| MissingParam
        Ok(entry.value)
    }

    ## Look up a request header by name (case-insensitive). (Factory:
    ## `Req.header(name)`.)
    get_header : { headers : List({ name : Str, value : Str }), .. }, Str -> Try(Str, [MissingHeader])
    get_header = |req, name| {
        wanted = Req.ascii_lower(name)
        entry = req.headers.find_first(|candidate| Req.ascii_lower(candidate.name) == wanted) ? |_| MissingHeader
        Ok(entry.value)
    }

    ## All query parameters, percent-decoded, in order of appearance.
    query_params : { query : Str, .. } -> List({ name : Str, value : Str })
    query_params = |req| Req.parse_pairs(req.query)

    ## Look up a single query parameter by name (first occurrence wins).
    ## (Factory: `Req.query(name)`.)
    get_query : { query : Str, .. }, Str -> Try(Str, [MissingQueryParam])
    get_query = |req, name| {
        entry = Req.parse_pairs(req.query).find_first(|candidate| candidate.name == name) ? |_| MissingQueryParam
        Ok(entry.value)
    }

    ## The request body as UTF-8 text.
    body_str : { body : List(U8), .. } -> Try(Str, [InvalidUtf8])
    body_str = |req| {
        text = Str.from_utf8(req.body) ? |_| InvalidUtf8
        Ok(text)
    }

    ## Decode the request body as JSON into any parseable type. The
    ## `InvalidJson` error carries the parse error (bare-tag `?` applies the
    ## mapping tag WITH the original error as its payload).
    json = |req| {
        text = Str.from_utf8(req.body) ? |_| InvalidUtf8
        value = Json.parse(text) ? InvalidJson
        Ok(value)
    }

    ## Parse an application/x-www-form-urlencoded body.
    form : { body : List(U8), .. } -> Try(List({ name : Str, value : Str }), [InvalidUtf8])
    form = |req| {
        text = Str.from_utf8(req.body) ? |_| InvalidUtf8
        Ok(Req.parse_pairs(text))
    }

    ## --- Coeffects -----------------------------------------------------------
    ##
    ## A coeffect is a PLAIN FUNCTION — `req => Try(a, Response)` — bound to a
    ## pure handler's positional arguments by `Cofx.withN`:
    ##
    ##     { method: "POST", path: "/greet", handler: Box.box(Cofx.with2(Req.json_body, Req.model, greet)) }
    ##     greet : { name : Str }, Model -> Response
    ##
    ## The handler receives the RAW resolved value — nothing to destructure.
    ## Parameterized coeffects are factories returning resolvers
    ## (`Req.param("id")`); app-defined coeffects are more functions of the
    ## same shape, pipelined with the arrow-call operator through the `Cofx`
    ## combinators (`->Cofx.and_then(…)->Cofx.map(…)`) or combined with the
    ## record builder (`{ a: ra, b: rb }.Cofx`). The built-ins below are pure
    ## projections of the request, resolved per-slot without memoization;
    ## `reserved_id!` is non-idempotent, so it memoizes through `Cofx.get!`
    ## and several slots in one request share ONE reservation. Pure resolvers
    ## are unmarked; effectful ones carry the compiler-enforced `!`.

    ## The raw request body bytes. Never rejects.
    body = |req| Ok(req.body)

    ## The request body as UTF-8 text. Rejects with 400 on invalid UTF-8.
    text_body = |req| {
        text = Req.body_str(req) ? |_| Req.reject(400, "request body is not valid UTF-8")
        Ok(text)
    }

    ## The request body decoded as JSON into the handler argument's type.
    ## Rejects with 400 when the body is not valid JSON for that shape.
    json_body = |req| {
        decoded = Req.json(req) ? |_| Req.reject(400, "request body is not valid JSON for the expected shape")
        Ok(decoded)
    }

    ## The request body parsed as application/x-www-form-urlencoded pairs.
    ## Rejects with 400 on invalid UTF-8.
    form_body = |req| {
        fields = Req.form(req) ? |_| Req.reject(400, "request body is not a valid urlencoded form")
        Ok(fields)
    }

    ## All query parameters, percent-decoded. Never rejects.
    query_pairs = |req| Ok(Req.query_params(req))

    ## All request headers. Never rejects.
    headers = |req| Ok(req.headers)

    ## The app's shared model, unboxed. Never rejects.
    model = |req| Ok(Box.unbox(req.model))

    ## Coeffect factory: the named path parameter. Rejects with 400 on a miss.
    ##
    ##     Cofx.with1(Req.param("id"), show_user)
    param = |name| |req| {
        value = Req.get_param(req, name) ? |_| Req.reject(400, "missing path parameter ${name}")
        Ok(value)
    }

    ## Coeffect factory: the named query parameter (first occurrence wins).
    ## Rejects with 400 on a miss.
    query = |name| |req| {
        value = Req.get_query(req, name) ? |_| Req.reject(400, "missing query parameter ${name}")
        Ok(value)
    }

    ## Coeffect factory: the named request header (case-insensitive). Rejects
    ## with 400 on a miss.
    header = |name| |req| {
        value = Req.get_header(req, name) ? |_| Req.reject(400, "missing header ${name}")
        Ok(value)
    }

    ## A freshly reserved, non-idempotent id — the canonical memoized
    ## coeffect. Bumps a per-name counter in a DB sequence table
    ## (`UPDATE ... RETURNING`) via `Host.sqlite_exec!`, creating the table on
    ## first use. Resolves THROUGH `Cofx.get!`, so several slots requesting it
    ## in one request share ONE reservation. The counter lives in the app's
    ## database, read from the model's `db : Str` field (any app using this
    ## coeffect must expose one).
    reserved_id! = |req| Cofx.get!(req.ctx, "coeffect.reserved_id", |{}| Req.reserve_fresh_id!(req))

    ## Internal: one un-memoized reservation (the `reserved_id!` cache miss).
    reserve_fresh_id! = |req| {
        db = Box.unbox(req.model).db
        _ = Host.sqlite_exec!({ db: db, sql: "CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, v INTEGER NOT NULL)", params: [] })
        _ = Host.sqlite_exec!({ db: db, sql: "INSERT INTO counters (name, v) VALUES ('reserved_id', 0) ON CONFLICT(name) DO NOTHING", params: [] })
        result = Host.sqlite_exec!({ db: db, sql: "UPDATE counters SET v = v + 1 WHERE name = 'reserved_id' RETURNING v", params: [] })
            ? |message| Req.reject(500, "reserved id: ${message}")
        reserved = Req.read_reserved_id(result) ? |_| Req.reject(500, "reserved id: sequence returned no value")
        Ok(reserved)
    }

    ## Internal: read the single INTEGER cell an `UPDATE ... RETURNING v`
    ## produces.
    read_reserved_id : { columns : List(Str), rows : List(List(Host.SqlValue)) } -> Try(I64, [NoValue])
    read_reserved_id = |result| {
        match result.rows.first() {
            Ok(row) => match row.first() {
                Ok(cell) => match cell {
                    Host.SqlValue.Integer(n) => Ok(n)
                    _ => Err(NoValue)
                }
                Err(_) => Err(NoValue)
            }
            Err(_) => Err(NoValue)
        }
    }

    ## Internal: a plain-text rejection response for coeffect failures.
    ## Left unannotated: naming the 4-field effectful Response type would need
    ## to reference a sibling platform module, which annotations can't do. The
    ## empty `effects: []` unifies with whatever the app's Response pins. The
    ## status is pinned to U16 explicitly — resolvers are top-level VALUES, so
    ## an unpinned numeral would default to Dec under the value restriction
    ## and poison their response type.
    reject = |status, message| {
        code : U16
        code = status
        {
            status: code,
            headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
            body: Str.to_utf8(message),
            effects: [],
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

import Host
import Cofx

expect Req.get_param({ params: [{ name: "id", value: "42" }] }, "id") == Ok("42")

expect Req.get_param({ params: [] }, "id") == Err(MissingParam)

expect Req.get_header({ headers: [{ name: "Content-Type", value: "text/plain" }] }, "content-type") == Ok("text/plain")

expect Req.get_header({ headers: [{ name: "x-a", value: "1" }] }, "X-A") == Ok("1")

expect Req.get_header({ headers: [] }, "accept") == Err(MissingHeader)

expect Req.query_params({ query: "a=1&b=two%20words" }) == [{ name: "a", value: "1" }, { name: "b", value: "two words" }]

expect Req.query_params({ query: "" }) == []

expect Req.query_params({ query: "flag" }) == [{ name: "flag", value: "" }]

expect Req.get_query({ query: "a=1&a=2" }, "a") == Ok("1")

expect Req.get_query({ query: "a=1" }, "b") == Err(MissingQueryParam)

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

# --- coeffect resolvers ---
#
# The built-ins are pure projections, so they are directly testable here.
# `reserved_id!` memoizes through the effectful `Cofx.get!` (which `expect`
# rejects); its run-once semantics are covered end to end by the e2e suite.

expect Req.body({ body: [104, 105] }) == Ok([104, 105])

expect Req.text_body({ body: [104, 105] }) == Ok("hi")

expect Req.text_body({ body: [255, 254] }) == Err(Req.reject(400, "request body is not valid UTF-8"))

expect {
    decoded : Try({ id : U64 }, _)
    decoded = Req.json_body({ body: "{\"id\":7}".to_utf8() })
    decoded == Ok({ id: 7 })
}

expect {
    decoded : Try({ id : U64 }, _)
    decoded = Req.json_body({ body: "nope".to_utf8() })
    decoded == Err(Req.reject(400, "request body is not valid JSON for the expected shape"))
}

expect Req.form_body({ body: "a=1&b=x+y".to_utf8() }) == Ok([{ name: "a", value: "1" }, { name: "b", value: "x y" }])

expect Req.query_pairs({ query: "a=1&b=two%20words" }) == Ok([{ name: "a", value: "1" }, { name: "b", value: "two words" }])

expect Req.headers({ headers: [{ name: "X-Custom", value: "abc" }] }) == Ok([{ name: "X-Custom", value: "abc" }])

expect Req.model({ model: Box.box({ greeting: "hey" }) }) == Ok({ greeting: "hey" })

# --- coeffect factories (parameterization = partial application) ---

expect {
    resolver = Req.param("id")
    resolver({ params: [{ name: "id", value: "42" }] }) == Ok("42")
}

expect {
    resolver = Req.param("id")
    match resolver({ params: [] }) {
        Ok(_) => False
        Err(response) => response.status == 400
    }
}

expect {
    resolver = Req.query("q")
    resolver({ query: "q=zed" }) == Ok("zed")
}

expect {
    resolver = Req.header("X-Custom")
    resolver({ headers: [{ name: "x-custom", value: "abc" }] }) == Ok("abc")
}

# NOTE: do NOT expect-test a composition like `Cofx.map(Req.param("id"), …)`
# here. Applying a sibling module's combinator to this module's factory inside
# an expect freezes the factory's request row to the expect's (closed) record,
# which then fails to unify with the full request record in downstream apps
# (compiler quirk, observed 2026-07-12). Composition over a factory is covered
# by the pure example's `fetched_user!` pipeline through the e2e gate.
