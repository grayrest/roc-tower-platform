app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req

# E2E test fixture app. No Port setting so the PORT env var controls binding.
Model : { prefix : Str, db_path : Str }

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

## App-defined extractor: pulls the {id} path parameter.
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
        Compression,
        CorsPermissive,
        TimeoutMs(10000),
        MaxBodyBytes(1024),
        ServeFiles({ url_prefix: "/static", dir: "examples/e2e/public" }),
    ],
    init!: init!,
    routes: [
        { method: "GET", path: "/", handler: Box.box(root!) },
        { method: "GET", path: "/users/{id}", handler: Box.box(Req.args1(user!)) },
        { method: "POST", path: "/echo", handler: Box.box(echo!) },
        { method: "GET", path: "/big", handler: Box.box(big!) },
        { method: "GET", path: "/model", handler: Box.box(model_route!) },
        { method: "GET", path: "/db", handler: Box.box(db!) },
        { method: "GET", path: "/query", handler: Box.box(query!) },
        { method: "POST", path: "/helper/greet", handler: Box.box(Req.args1(helper_greet!)) },
        { method: "POST", path: "/helper/form", handler: Box.box(Req.args1(helper_form!)) },
        { method: "GET", path: "/helper/q", handler: Box.box(helper_q!) },
        { method: "GET", path: "/helper/redirect", handler: Box.box(helper_redirect!) },
        { method: "GET", path: "/helper/header", handler: Box.box(Req.args1(helper_header!)) },
        { method: "GET", path: "/helper/combo/{id}", handler: Box.box(Req.args3(helper_combo!)) },
        # Phase 3: host-executed effects.
        { method: "GET", path: "/effects/insert/{tag}", handler: Box.box(effects_insert!) },
        { method: "GET", path: "/effects/rollback", handler: Box.box(effects_rollback!) },
        { method: "GET", path: "/effects/rollback-check", handler: Box.box(effects_rollback_check!) },
        { method: "GET", path: "/effects/opt", handler: Box.box(effects_opt!) },
        { method: "GET", path: "/effects/opt-check", handler: Box.box(effects_opt_check!) },
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

user! : UserId => Response
user! = |UserId(id)| text(200, "user ${id}")

echo! : Request => Response
echo! = |req| {
    status: 200,
    headers: [{ name: "content-type", value: "application/octet-stream" }],
    body: req.body,
    effects: [],
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
    effects: [],
}

helper_greet! : Req.JsonBody({ name : Str }) => Response
helper_greet! = |Req.JsonBody.JsonBody(body)| Res.json(200, { message: "hello, ${body.name}" })

helper_form! : Req.FormBody => Response
helper_form! = |Req.FormBody.FormBody(fields)| {
    rendered = fields.map(|field| "${field.name}=${field.value}")
    Res.text(200, Str.join_with(rendered, ";"))
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

helper_header! : Req.Headers => Response
helper_header! = |headers_arg| {
    match headers_arg.get("x-custom") {
        Ok(value) => Res.text(200, "x-custom=${value}")
        Err(_) => Res.text(400, "missing header")
    }
}

# Destructure the payload wrappers (UserId, Model); keep QueryParams as a
# named arg so its .get() accessor stays available.
helper_combo! : UserId, Req.QueryParams, Req.Model(Model) => Response
helper_combo! = |UserId(id), query_arg, Req.Model.Model(model)| {
    q = match query_arg.get("q") {
        Ok(value) => value
        Err(_) => "none"
    }
    Res.text(200, "id=${id};q=${q};prefix=${model.prefix}")
}

# --- Phase 3 effect fixtures ---
#
# These handlers return a Must/Opt effect list the host executes AFTER the
# handler. DDL runs inline via Host.sqlite_exec! (autocommit, before the
# effect transaction); the actual write is the returned Must(DbWrite).

## Must(DbWrite) into a UNIQUE column. First insert of a tag succeeds and the
## handler's own 200 comes back; a duplicate fails the effect, so the host
## invokes the boxed err_handler and returns its 409 instead.
effects_insert! : Request => Response
effects_insert! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    tag = match Req.param(req, "tag") {
        Ok(value) => value
        Err(_) => "x"
    }
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS effect_users (email TEXT UNIQUE NOT NULL)", params: [] })
    {
        status: 200,
        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
        body: "inserted ${tag}".to_utf8(),
        effects: [
            Res.must(
                DbWrite({ db: db_path, sql: "INSERT INTO effect_users (email) VALUES (?)", params: [Host.SqlValue.Text(tag)] }),
                |err| match err {
                    UniqueViolation(_) => {
                        status: 409,
                        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
                        body: "duplicate ${tag}".to_utf8(),
                    }
                    _ => {
                        status: 500,
                        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
                        body: "insert failed".to_utf8(),
                    }
                },
            ),
        ],
    }
}

## Two ordered Must(DbWrite): the first inserts a fresh row, the second
## collides on UNIQUE. The whole transaction must roll back, so the fresh row
## is absent afterwards, and the second effect's err_handler returns 409.
effects_rollback! : Request => Response
effects_rollback! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS effect_rollback (val TEXT UNIQUE NOT NULL)", params: [] })
    _ = Host.sqlite_exec!({ db: db_path, sql: "DELETE FROM effect_rollback WHERE val = 'rollback_fresh'", params: [] })
    _ = Host.sqlite_exec!({ db: db_path, sql: "INSERT OR IGNORE INTO effect_rollback (val) VALUES ('rollback_dup')", params: [] })
    on_dup = |err| match err {
        UniqueViolation(_) => {
            status: 409,
            headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
            body: "rolled back".to_utf8(),
        }
        _ => {
            status: 500,
            headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
            body: "unexpected".to_utf8(),
        }
    }
    {
        status: 200,
        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
        body: "should never be seen".to_utf8(),
        effects: [
            Res.must(DbWrite({ db: db_path, sql: "INSERT INTO effect_rollback (val) VALUES ('rollback_fresh')", params: [] }), on_dup),
            Res.must(DbWrite({ db: db_path, sql: "INSERT INTO effect_rollback (val) VALUES ('rollback_dup')", params: [] }), on_dup),
        ],
    }
}

## Reports whether the rolled-back 'rollback_fresh' row is present.
effects_rollback_check! : Request => Response
effects_rollback_check! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    match Host.sqlite_exec!({ db: db_path, sql: "SELECT COUNT(*) FROM effect_rollback WHERE val = 'rollback_fresh'", params: [] }) {
        Ok(result) => text(200, "fresh_rows=${count_of(result)}")
        Err(message) => text(500, "db error: ${message}")
    }
}

## Opt(Log) plus an observable Opt(DbWrite): both fire, neither changes the
## 200 status the handler returned.
effects_opt! : Request => Response
effects_opt! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS effect_opt (id INTEGER PRIMARY KEY AUTOINCREMENT)", params: [] })
    {
        status: 200,
        headers: [{ name: "content-type", value: "text/plain; charset=utf-8" }],
        body: "opt ok".to_utf8(),
        effects: [
            Res.opt(Log("optional log fired")),
            Res.opt(DbWrite({ db: db_path, sql: "INSERT INTO effect_opt DEFAULT VALUES", params: [] })),
        ],
    }
}

## Reports how many rows the Opt(DbWrite) inserted.
effects_opt_check! : Request => Response
effects_opt_check! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    match Host.sqlite_exec!({ db: db_path, sql: "SELECT COUNT(*) FROM effect_opt", params: [] }) {
        Ok(result) => text(200, "opt_rows=${count_of(result)}")
        Err(message) => text(500, "db error: ${message}")
    }
}

## Read an integer COUNT(*) out of a single-cell result.
count_of : { columns : List(Str), rows : List(List(Host.SqlValue)) } -> Str
count_of = |result| {
    match result.rows.first() {
        Ok(row) => match row.first() {
            Ok(cell) => match cell {
                Host.SqlValue.Integer(n) => n.to_str()
                _ => "?"
            }
            Err(_) => "?"
        }
        Err(_) => "?"
    }
}
