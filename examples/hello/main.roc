app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req

Model : { greeting : Str, started_at_ms : U64 }

Request : {
    method : Str,
    path : Str,
    params : List({ name : Str, value : Str }),
    query : Str,
    headers : List({ name : Str, value : Str }),
    body : List(U8),
    model : Box(Model),
}

Response : {
    status : U16,
    headers : List({ name : Str, value : Str }),
    body : List(U8),
}

## A custom extractor: any nominal type with a from_request method can be a
## handler argument. Rejections (Err) are returned as the response.
UserId := [UserId(Str)].{
    from_request : Request -> Try(UserId, Response)
    from_request = |req| {
        match Req.param(req, "id") {
            Ok(value) => Ok(UserId.UserId(value))
            Err(_) => Err(Res.text(400, "missing id path parameter"))
        }
    }
}

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
        { method: "GET", path: "/", handler: Box.box(Req.args1(home!)) },
        { method: "GET", path: "/users/{id}", handler: Box.box(Req.args1(user!)) },
        { method: "POST", path: "/echo", handler: Box.box(Req.args1(echo!)) },
        { method: "POST", path: "/greet", handler: Box.box(Req.args2(greet!)) },
        { method: "GET", path: "/uptime", handler: Box.box(Req.args1(uptime!)) },
        { method: "GET", path: "/random", handler: Box.box(random!) },
        { method: "GET", path: "/proxy", handler: Box.box(proxy!) },
        { method: "GET", path: "/db", handler: Box.box(db!) },
    ],
}

init! : () => Try(Model, Str)
init! = || {
    Host.log!("initializing hello app")
    greeting = match Host.env_var!("GREETING") {
        Ok(value) => value
        Err(_) => "Hello"
    }
    Ok({ greeting: greeting, started_at_ms: Host.now_ms!() })
}

home! : Req.Model(Model) => Response
home! = |model_arg| Res.text(200, "${model_arg.value().greeting}, world!")

user! : UserId => Response
user! = |id_arg| {
    id = match id_arg { UserId.UserId(value) => value }
    Res.text(200, "user ${id}")
}

echo! : Req.Body => Response
echo! = |body_arg| Res.bytes(200, "application/octet-stream", body_arg.value())

## POST a JSON body like {"name":"Ada"} and get JSON back.
greet! : Req.JsonBody({ name : Str }), Req.Model(Model) => Response
greet! = |body_arg, model_arg| {
    Res.json(200, { message: "${model_arg.value().greeting}, ${body_arg.value().name}" })
}

uptime! : Req.Model(Model) => Response
uptime! = |model_arg| {
    Res.text(200, "uptime_ms=${(Host.now_ms!() - model_arg.value().started_at_ms).to_str()}")
}

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

db! : Request => Response
db! = |_req| {
    db_path = "/tmp/tower-platform-demo.db"
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS visits (id INTEGER PRIMARY KEY AUTOINCREMENT, at_ms INTEGER)", params: [] })
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
