app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Cofx

Model : { greeting : Str }

Request : {
    ctx : U64,
    method : Str,
    path : Str,
    params : List({ name : Str, value : Str }),
    query : Str,
    headers : List({ name : Str, value : Str }),
    body : List(U8),
    model : Box(Model),
}

Response : Res.Response

db_path : Str
db_path = "/tmp/tower-platform-demo.db"

main = |{}| {
    settings: [
        Port(8000),
        Compression,
        CorsPermissive,
        TimeoutMs(10000),
        ServeFiles({ url_prefix: "/static", dir: "examples/hello/public" }),
    ],
    init!: init!,
    routes: [
        # Sanctioned style: pure `->` handlers, coeffects in, Response out.
        # A coeffect is a resolver VALUE (`req => Try(a, resp)`); Cofx.withN
        # binds them to the handler's positional args. Parameterizing one is
        # partial application: `Req.param("id")` is a resolver for that
        # specific path parameter.
        { method: "GET", path: "/", handler: Box.box(Cofx.with1(Req.model, home)) },
        { method: "GET", path: "/users/{id}", handler: Box.box(Cofx.with1(Req.param("id"), user)) },
        { method: "POST", path: "/echo", handler: Box.box(Cofx.with1(Req.body, echo)) },
        { method: "POST", path: "/greet", handler: Box.box(Cofx.with2(Req.json_body, Req.model, greet)) },
        { method: "GET", path: "/visit", handler: Box.box(Cofx.with0(visit)) },
        # Escape hatch: raw effectful `Request => Response` handlers, for logic
        # that interleaves I/O with response-building. Still supported; not the
        # default. See examples/realworld for the fully pure style end to end.
        { method: "GET", path: "/random", handler: Box.box(random!) },
        { method: "GET", path: "/proxy", handler: Box.box(proxy!) },
        { method: "GET", path: "/db", handler: Box.box(db!) },
    ],
}

init! : () => Try(Model, Str)
init! = || {
    Host.log!("initializing hello app")
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS visits (id INTEGER PRIMARY KEY AUTOINCREMENT, at_ms INTEGER)", params: [] })
    greeting = match Host.env_var!("GREETING") {
        Ok(value) => value
        Err(_) => "Hello"
    }
    Ok({ greeting: greeting })
}

# --- Pure handlers (`->`: the compiler forbids any I/O in these bodies) ------
# Handlers receive the RAW resolved values — no wrapper types to destructure.

home : Model -> Response
home = |model| Res.text(200, "${model.greeting}, world!")

## The `{id}` path parameter, resolved by the `Req.param("id")` factory.
user : Str -> Response
user = |id| Res.text(200, "user ${id}")

echo : List(U8) -> Response
echo = |bytes| Res.bytes(200, "application/octet-stream", bytes)

## POST a JSON body like {"name":"Ada"} and get JSON back.
greet : { name : Str }, Model -> Response
greet = |body, model|
    Res.json(200, { message: "${model.greeting}, ${body.name}" })

## Effect showcase: a pure handler records a visit via a Must(DbWrite) effect
## that the host runs (transactionally) after the handler returns. The Res.fx
## builder chains effects and folds them into the terminal response. No Host.*!
## call appears in the body — it wouldn't compile.
visit = ||
    Res.fx(DbWrite({ db: db_path, sql: "INSERT INTO visits (at_ms) VALUES (0)", params: [] }))
       .opt(Log("recorded a visit"))
       .text(201, "visit recorded")

# --- Escape hatch: raw effectful handlers -----------------------------------

random! : Request => Response
random! = |_req| Res.text(200, "random=${Str.inspect(Host.random_bytes!(8))}")

proxy! : Request => Response
proxy! = |_req| {
    result = Host.http_send!({
        method: "GET",
        url: "http://127.0.0.1:8000/users/proxied",
        headers: [],
        body: [],
        timeout_ms: 5000,
    })
    match result {
        Ok(response) => Res.text(200, "proxied: status=${response.status.to_str()} body=${Str.from_utf8_lossy(response.body)}")
        Err(message) => Res.text(502, "proxy failed: ${message}")
    }
}

## Read-modify-read is naturally imperative — a good escape-hatch case.
db! : Request => Response
db! = |_req| {
    _ = Host.sqlite_exec!({ db: db_path, sql: "INSERT INTO visits (at_ms) VALUES (?)", params: [Host.SqlValue.Integer(Host.now_ms!().to_i64_wrap())] })
    match Host.sqlite_exec!({ db: db_path, sql: "SELECT COUNT(*) AS n FROM visits", params: [] }) {
        Ok(result) => {
            count = match result.rows.first() {
                Ok(row) => match row.first() {
                    Ok(cell) => match cell {
                        Host.SqlValue.Integer(n) => n.to_str()
                        _ => "?"
                    }
                    Err(_) => "?"
                }
                Err(_) => "?"
            }
            Res.text(200, "visits=${count}")
        }
        Err(message) => Res.text(500, "db error: ${message}")
    }
}
