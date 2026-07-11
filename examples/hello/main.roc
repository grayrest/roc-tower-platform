app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host

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
        { method: "GET", path: "/", handler: Box.box(home!) },
        { method: "GET", path: "/users/{id}", handler: Box.box(user!) },
        { method: "POST", path: "/echo", handler: Box.box(echo!) },
        { method: "GET", path: "/uptime", handler: Box.box(uptime!) },
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

home! : Request => Response
home! = |req| {
    model = Box.unbox(req.model)
    text(200, "${model.greeting}, world!")
}

user! : Request => Response
user! = |req| {
    id = match req.params.first() {
        Ok(param) => param.value
        Err(_) => "unknown"
    }
    text(200, "user ${id}")
}

echo! : Request => Response
echo! = |req| {
    status: 200,
    headers: [{ name: "content-type", value: "application/octet-stream" }],
    body: req.body,
}

uptime! : Request => Response
uptime! = |req| {
    model = Box.unbox(req.model)
    text(200, "uptime_ms=${(Host.now_ms!() - model.started_at_ms).to_str()}")
}

random! : Request => Response
random! = |_req| {
    bytes = Host.random_bytes!(8)
    text(200, "random=${Str.inspect(bytes)}")
}

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
        Ok(response) => text(200, "proxied: status=${response.status.to_str()} body=${Str.from_utf8_lossy(response.body)}")
        Err(message) => text(502, "proxy failed: ${message}")
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
            text(200, "visits=${count}")
        }
        Err(message) => text(500, "db error: ${message}")
    }
}

text : U16, Str -> Response
text = |status, message| {
    status: status,
    headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
    body: Str.to_utf8(message),
}
