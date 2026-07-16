app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Cofx
import pf.Sql

# E2E test fixture app. No Port setting so the PORT env var controls binding.
Model : { prefix : Str, db_path : Str }

Request : {
    ctx : U64,
    method : Str,
    path : Str,
    params : List({ name : Str, value : Str }),
    query : Str,
    headers : List({ name : Str, value : Str }),
    model : Box(Model),
}

Response : Res.Response

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
        { method: "GET", path: "/users/{id}", handler: Box.box(Cofx.with1(Req.param("id"), user)) },
        { method: "POST", path: "/echo", handler: Box.box(echo!) },
        { method: "GET", path: "/big", handler: Box.box(big!) },
        { method: "GET", path: "/model", handler: Box.box(model_route!) },
        { method: "GET", path: "/db", handler: Box.box(db!) },
        { method: "GET", path: "/query", handler: Box.box(query!) },
        { method: "POST", path: "/helper/greet", handler: Box.box(Cofx.with1(Req.json_body!, helper_greet)) },
        { method: "POST", path: "/helper/form", handler: Box.box(Cofx.with1(Req.form_body!, helper_form)) },
        { method: "GET", path: "/helper/q", handler: Box.box(helper_q!) },
        { method: "GET", path: "/helper/redirect", handler: Box.box(helper_redirect!) },
        { method: "GET", path: "/helper/header", handler: Box.box(Cofx.with1(Req.header("x-custom"), helper_header)) },
        { method: "GET", path: "/helper/combo/{id}", handler: Box.box(Cofx.with3(Req.param("id"), Req.query_pairs, Req.model, helper_combo)) },
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

user : Str -> Response
user = |id| text(200, "user ${id}")

echo! : Request => Response
echo! = |req| {
    status: 200,
    headers: [{ name: "content-type", value: "application/octet-stream" }],
    # The body is not a request-record field — extract it via the host
    # (which also tells the response cache this route varies on it, D14).
    body: Host.request_body!(req.ctx),
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
    _ = Sql.migrate!(db_path, "CREATE TABLE IF NOT EXISTS hits (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
    _ = Sql.migrate!(db_path, "INSERT INTO hits DEFAULT VALUES", [])
    match Sql.query!(req.ctx, db_path, "SELECT COUNT(*) AS n FROM hits", []) {
        Ok(rows) => text(200, "hits=${count_of(rows)}")
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

helper_greet : { name : Str } -> Response
helper_greet = |body| Res.json(200, { message: "hello, ${body.name}" })

helper_form : List({ name : Str, value : Str }) -> Response
helper_form = |fields| {
    rendered = fields.map(|field| "${field.name}=${field.value}")
    Res.text(200, Str.join_with(rendered, ";"))
}

helper_q! : Request => Response
helper_q! = |req| Res.from_try(helper_q_impl!(req))

helper_q_impl! : Request => Try(Response, Response)
helper_q_impl! = |req| {
    match Req.get_query(req, "name") {
        Ok(name) => Ok(Res.text(200, "q=${name}"))
        Err(_) => Err(Res.text(400, "missing name"))
    }
}

helper_redirect! : Request => Response
helper_redirect! = |_req| Res.redirect(302, "/")

## The `Req.header("x-custom")` factory resolves (or 400-rejects) the
## header before this runs, so the handler sees the raw value.
helper_header : Str -> Response
helper_header = |value| Res.text(200, "x-custom=${value}")

helper_combo : Str, List({ name : Str, value : Str }), Model -> Response
helper_combo = |id, query_pairs, model| {
    q = match query_pairs.find_first(|entry| entry.name == "q") {
        Ok(entry) => entry.value
        Err(_) => "none"
    }
    Res.text(200, "id=${id};q=${q};prefix=${model.prefix}")
}

# --- Phase 3 effect fixtures ---
#
# These handlers return a Must/Opt effect list the host executes AFTER the
# handler. DDL runs inline via Sql.migrate! (autocommit, before the
# effect transaction); the actual write is the returned Must(DbWrite).

## Must(DbWrite) into a UNIQUE column. First insert of a tag succeeds and the
## handler's own 200 comes back; a duplicate fails the effect, so the host
## invokes the boxed err_handler and returns its 409 instead.
effects_insert! : Request => Response
effects_insert! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    tag = match Req.get_param(req, "tag") {
        Ok(value) => value
        Err(_) => "x"
    }
    _ = Sql.migrate!(db_path, "CREATE TABLE IF NOT EXISTS effect_users (email TEXT UNIQUE NOT NULL)", [])
    Res.fx_err(
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
        )
        .text(200, "inserted ${tag}")
}

## Two ordered Must(DbWrite): the first inserts a fresh row, the second
## collides on UNIQUE. The whole transaction must roll back, so the fresh row
## is absent afterwards, and the second effect's err_handler returns 409.
effects_rollback! : Request => Response
effects_rollback! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    _ = Sql.migrate!(db_path, "CREATE TABLE IF NOT EXISTS effect_rollback (val TEXT UNIQUE NOT NULL)", [])
    _ = Sql.migrate!(db_path, "DELETE FROM effect_rollback WHERE val = 'rollback_fresh'", [])
    _ = Sql.migrate!(db_path, "INSERT OR IGNORE INTO effect_rollback (val) VALUES ('rollback_dup')", [])
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
    Res.fx_err(DbWrite({ db: db_path, sql: "INSERT INTO effect_rollback (val) VALUES ('rollback_fresh')", params: [] }), on_dup)
        .fx_err(DbWrite({ db: db_path, sql: "INSERT INTO effect_rollback (val) VALUES ('rollback_dup')", params: [] }), on_dup)
        .text(200, "should never be seen")
}

## Reports whether the rolled-back 'rollback_fresh' row is present.
effects_rollback_check! : Request => Response
effects_rollback_check! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    match Sql.query!(req.ctx, db_path, "SELECT COUNT(*) AS n FROM effect_rollback WHERE val = 'rollback_fresh'", []) {
        Ok(rows) => text(200, "fresh_rows=${count_of(rows)}")
        Err(message) => text(500, "db error: ${message}")
    }
}

## Opt(Log) plus an observable Opt(DbWrite): both fire, neither changes the
## 200 status the handler returned.
effects_opt! : Request => Response
effects_opt! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    _ = Sql.migrate!(db_path, "CREATE TABLE IF NOT EXISTS effect_opt (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
    Res.fx_opt(Log("optional log fired"))
        .opt(DbWrite({ db: db_path, sql: "INSERT INTO effect_opt DEFAULT VALUES", params: [] }))
        .text(200, "opt ok")
}

## Reports how many rows the Opt(DbWrite) inserted.
effects_opt_check! : Request => Response
effects_opt_check! = |req| {
    model = Box.unbox(req.model)
    db_path = model.db_path
    match Sql.query!(req.ctx, db_path, "SELECT COUNT(*) AS n FROM effect_opt", []) {
        Ok(rows) => text(200, "opt_rows=${count_of(rows)}")
        Err(message) => text(500, "db error: ${message}")
    }
}

## Read the integer `n` COUNT column out of a single-row result.
count_of : List(Sql.Row) -> Str
count_of = |rows| {
    match Sql.first(rows) {
        Ok(row) => match row.i64("n") {
            Ok(n) => n.to_str()
            Err(_) => "?"
        }
        Err(_) => "?"
    }
}
