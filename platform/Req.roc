## Request inspection: pure helpers for raw handlers, and the built-in
## coeffect resolvers/factories for pure handlers (see `Ctx`).
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

    ## --- Coeffects -----------------------------------------------------------
    ##
    ## A coeffect is a `Resolver` value bound to a pure handler's positional
    ## arguments by `Ctx.withN`:
    ##
    ##     { method: "POST", path: "/greet", handler: Box.box(Ctx.with2(Req.json_body, Req.model, greet)) }
    ##     greet : { name : Str }, Model -> Response
    ##
    ## The handler receives the RAW resolved value — nothing to destructure.
    ## Parameterized coeffects are factories returning resolvers
    ## (`Req.param_named("id")`); app-defined coeffects wrap a function with
    ## `Resolver.from`, chain with `.map` / `.and_then`, and combine with the
    ## record builder (`{ a: ra, b: rb }.Resolver`). The built-ins below are
    ## pure projections of the request, resolved per-slot without memoization;
    ## `reserved_id` is non-idempotent, so it memoizes through `Ctx.get!` and
    ## several slots in one request share ONE reservation.

    ## The raw request body bytes. Never rejects.
    body = Resolver.from(|req| Ok(req.body))

    ## The request body as UTF-8 text. Rejects with 400 on invalid UTF-8.
    text_body = Resolver.from(|req| match Req.body_str(req) {
        Ok(text) => Ok(text)
        Err(_) => Err(Req.reject(400, "request body is not valid UTF-8"))
    })

    ## Coeffect factory: the request body decoded as JSON into the handler
    ## argument's type. Rejects with 400 when the body is not valid JSON for
    ## that shape. A factory (`Req.json_body()`) rather than a constant — its
    ## result type is parameterized by the handler's, and a polymorphic
    ## constant would trip the compiler's value restriction.
    json_body = || Resolver.from(|req| match Req.json(req) {
        Ok(decoded) => Ok(decoded)
        Err(_) => Err(Req.reject(400, "request body is not valid JSON for the expected shape"))
    })

    ## The request body parsed as application/x-www-form-urlencoded pairs.
    ## Rejects with 400 on invalid UTF-8.
    form_body = Resolver.from(|req| match Req.form(req) {
        Ok(fields) => Ok(fields)
        Err(_) => Err(Req.reject(400, "request body is not a valid urlencoded form"))
    })

    ## All query parameters, percent-decoded. Never rejects.
    query_pairs = Resolver.from(|req| Ok(Req.query_params(req)))

    ## All request headers. Never rejects.
    headers = Resolver.from(|req| Ok(req.headers))

    ## The app's shared model, unboxed. Never rejects.
    model = Resolver.from(|req| Ok(Box.unbox(req.model)))

    ## Coeffect factory: the named path parameter. Rejects with 400 on a miss.
    ##
    ##     Ctx.with1(Req.param_named("id"), show_user)
    param_named = |name| Resolver.from(|req| match Req.param(req, name) {
        Ok(value) => Ok(value)
        Err(_) => Err(Req.reject(400, "missing path parameter ${name}"))
    })

    ## Coeffect factory: the named query parameter (first occurrence wins).
    ## Rejects with 400 on a miss.
    query_named = |name| Resolver.from(|req| match Req.query(req, name) {
        Ok(value) => Ok(value)
        Err(_) => Err(Req.reject(400, "missing query parameter ${name}"))
    })

    ## Coeffect factory: the named request header (case-insensitive). Rejects
    ## with 400 on a miss.
    header_named = |name| Resolver.from(|req| match Req.header(req, name) {
        Ok(value) => Ok(value)
        Err(_) => Err(Req.reject(400, "missing header ${name}"))
    })

    ## A freshly reserved, non-idempotent id — the canonical memoized
    ## coeffect. Bumps a per-name counter in a DB sequence table
    ## (`UPDATE ... RETURNING`) via `Host.sqlite_exec!`, creating the table on
    ## first use. Resolves THROUGH `Ctx.get!`, so several slots requesting it
    ## in one request share ONE reservation. The counter lives in the app's
    ## database, read from the model's `db : Str` field (any app using this
    ## coeffect must expose one).
    reserved_id = Resolver.from(|req| Ctx.get!(req.ctx, "coeffect.reserved_id", |{}| Req.reserve_fresh_id!(req)))

    ## Internal: one un-memoized reservation (the `reserved_id` cache miss).
    reserve_fresh_id! = |req| {
        db = Box.unbox(req.model).db
        _ = Host.sqlite_exec!({ db: db, sql: "CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, v INTEGER NOT NULL)", params: [] })
        _ = Host.sqlite_exec!({ db: db, sql: "INSERT INTO counters (name, v) VALUES ('reserved_id', 0) ON CONFLICT(name) DO NOTHING", params: [] })
        match Host.sqlite_exec!({ db: db, sql: "UPDATE counters SET v = v + 1 WHERE name = 'reserved_id' RETURNING v", params: [] }) {
            Ok(result) => match Req.read_reserved_id(result) {
                Ok(reserved) => Ok(reserved)
                Err(_) => Err(Req.reject(500, "reserved id: sequence returned no value"))
            }
            Err(message) => Err(Req.reject(500, "reserved id: ${message}"))
        }
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
import Ctx
import Resolver

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

# NOTE: the coeffect resolvers above are not expect-testable — a `Resolver`
# wraps a `req => Try(a, resp)` payload, so running one counts as effectful
# and `expect` rejects it (even when the wrapped function is pure). Their
# parsing logic IS unit-tested through the pure helpers above
# (param/header/query/body_str/json/form); resolution, rejection statuses,
# and `reserved_id`'s run-once memoization are covered by the e2e suite.
