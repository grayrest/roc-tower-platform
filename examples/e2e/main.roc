app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req

# E2E test fixture app. No Port setting so the PORT env var controls binding.
Model : { prefix : Str, db_path : Str }

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
        Compression,
        CorsPermissive,
        TimeoutMs(10000),
        MaxBodyBytes(1024),
        ServeFiles({ url_prefix: "/static", dir: "examples/e2e/public" }),
    ],
    init!: init!,
    routes: [
        { method: "GET", path: "/", handler: Box.box(root!) },
        { method: "GET", path: "/users/{id}", handler: Box.box(user!) },
        { method: "POST", path: "/echo", handler: Box.box(echo!) },
        { method: "GET", path: "/big", handler: Box.box(big!) },
        { method: "GET", path: "/model", handler: Box.box(model_route!) },
        { method: "GET", path: "/db", handler: Box.box(db!) },
        { method: "GET", path: "/query", handler: Box.box(query!) },
        { method: "POST", path: "/helper/greet", handler: Box.box(helper_greet!) },
        { method: "POST", path: "/helper/form", handler: Box.box(helper_form!) },
        { method: "GET", path: "/helper/q", handler: Box.box(helper_q!) },
        { method: "GET", path: "/helper/redirect", handler: Box.box(helper_redirect!) },
        { method: "GET", path: "/helper/header", handler: Box.box(helper_header!) },
    ],
}

init! : () => Try(Model, Str)
init! = || {
    prefix = match Host.env_var!("E2E_PREFIX") {
        Ok(value) => value
        Err(_) => "default-prefix"
    }
    db_path = match Host.env_var!("E2E_DB_PATH") {
        Ok(value) => value
        Err(_) => "/tmp/tower-platform-e2e.db"
    }
    Ok({ prefix: prefix, db_path: db_path })
}

root! : Request => Response
root! = |_req| text(200, "root ok")

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

big! : Request => Response
big! = |_req| {
    chunk = "tower-platform compressible payload "
    seeds = [1, 2, 3, 4, 5, 6, 7]
    body = seeds.fold(chunk, |acc, _| Str.concat(acc, acc))
    text(200, body)
}

model_route! : Request => Response
model_route! = |req| {
    model = Box.unbox(req.model)
    text(200, "prefix=${model.prefix}")
}

db! : Request => Response
db! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS hits (id INTEGER PRIMARY KEY AUTOINCREMENT)", params: [] })
    _ = Host.sqlite_exec!({ db: db_path, sql: "INSERT INTO hits DEFAULT VALUES", params: [] })
    match Host.sqlite_exec!({ db: db_path, sql: "SELECT COUNT(*) FROM hits", params: [] }) {
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
            text(200, "hits=${count}")
        }
        Err(message) => text(500, "db error: ${message}")
    }
}

query! : Request => Response
query! = |req| text(200, "query=${req.query}")

text : U16, Str -> Response
text = |status, message| {
    status: status,
    headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
    body: Str.to_utf8(message),
}

helper_greet! : Request => Response
helper_greet! = |req| {
    parsed : Try({ name : Str }, _)
    parsed = Req.json(req)
    match parsed {
        Ok(body) => Res.json(200, { message: "hello, ${body.name}" })
        Err(_) => Res.text(400, "bad json")
    }
}

helper_form! : Request => Response
helper_form! = |req| {
    match Req.form(req) {
        Ok(fields) => {
            rendered = fields.map(|field| "${field.name}=${field.value}")
            Res.text(200, Str.join_with(rendered, ";"))
        }
        Err(_) => Res.text(400, "bad form")
    }
}

helper_q! : Request => Response
helper_q! = |req| Res.from_try(helper_q_impl!(req))

helper_q_impl! : Request => Try(Response, Response)
helper_q_impl! = |req| {
    match Req.query(req, "name") {
        Ok(name) => Ok(Res.text(200, "q=${name}"))
        Err(_) => Err(Res.text(400, "missing name"))
    }
}

helper_redirect! : Request => Response
helper_redirect! = |_req| Res.redirect(302, "/")

helper_header! : Request => Response
helper_header! = |req| {
    match Req.header(req, "x-custom") {
        Ok(value) => Res.text(200, "x-custom=${value}")
        Err(_) => Res.text(400, "missing header")
    }
}
