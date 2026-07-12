app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Cofx

# Reference PURE app: coeffects in, a pure handler that returns a Response
# carrying host-run effects. No handler calls Host.*! directly — reads are
# coeffects (resolver values, run before the handler), writes are effects
# (run after). Handlers receive RAW resolved values.
Model : { db : Str }

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

# One fixed database file for the whole app. The e2e test clears it before the
# run. The handlers use the same constant in their DbWrite effects;
# `Req.reserved_id!` reads it from the model.
db_path : Str
db_path = "/tmp/tower-platform-pure-e2e.db"

# Local alias so annotations can drop the `Host.` prefix (bare tags below
# already resolve without it).
SqlValue : Host.SqlValue

## An app-defined coeffect FACTORY, parameterized by which path param names
## the user id — `user_from!("id")` — and built as a PIPELINE: a
## record-builder block gathers the path param and the db path (the
## `Req.model` resolver unboxes the model; `->Cofx.map` projects the
## field), then the arrow-call operator (`x->f(y)` is `f(x, y)`) chains into
## the dependent DB load via `Cofx.resolve` (the step needs only the gathered
## values, not the request — use `Cofx.and_then` when it needs both). A read
## miss rejects with 404, so the pure handler only ever sees a present user.
## The factory itself is pure (it only BUILDS the resolver); the `!`
## documents that what it returns does effectful reads.
user_from! : Str -> (Request => Try({ id : I64, name : Str }, Response))
user_from! = |param_name| {
        id: Req.param(param_name),
        db: Req.model->Cofx.map(|model| model.db),
    }.Cofx
    ->Cofx.resolve(|{id, db}| {
        match Host.sqlite_exec!({ db: db, sql: "SELECT id, name FROM users WHERE id = CAST(? AS INTEGER)", params: [Text(id)] }) {
            Ok(result) => match read_user_row(result) {
                Ok(user) => Ok(user)
                Err(_) => Err(Res.text(404, "user ${id} not found"))
            }
            Err(message) => Err(Res.text(500, "db error: ${message}"))
        }
    })

## Pure row decode for `user_from!`.
read_user_row : { columns : List(Str), rows : List(List(SqlValue)) } -> Try({ id : I64, name : Str }, [NotFound])
read_user_row = |result| {
    match result.rows.first() {
        Ok(row) => {
            id = match row.get(0) {
                Ok(cell) => match cell {
                    Integer(n) => n
                    _ => -1
                }
                Err(_) => -1
            }
            name = match row.get(1) {
                Ok(cell) => match cell {
                    Text(s) => s
                    _ => ""
                }
                Err(_) => ""
            }
            Ok({ id: id, name: name })
        }
        Err(_) => Err(NotFound)
    }
}

main = |{}| {
    settings: [
        CorsPermissive,
        TimeoutMs(10000),
        MaxBodyBytes(4096),
    ],
    init!: init!,
    routes: [
        # POST /users: reserve an id (non-idempotent, memoized coeffect) +
        # decode the JSON body, then a pure handler returns a Must(DbWrite)
        # with a UniqueViolation -> 409 err_handler and an Opt(Log).
        { method: "POST", path: "/users", handler: Box.box(Cofx.with2(Req.reserved_id!, Req.json_body, create_user)) },
        # GET /users/{id}: the pipelined read coeffect loads the row (404 miss).
        { method: "GET", path: "/users/{id}", handler: Box.box(Cofx.with1(user_from!("id"), get_user)) },
        # Record-builder coeffect: two reserved_id! slots combined into one
        # NAMED block (desugars through Cofx.map2) — and they still share
        # ONE reservation, because the memo runs underneath.
        { method: "GET", path: "/reserve-twice", handler: Box.box(Cofx.with1({ a: Req.reserved_id!, b: Req.reserved_id! }.Cofx, reserve_twice)) },
    ],
}

init! : () => Try(Model, Str)
init! = || {
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)", params: [] })
    Ok({ db: db_path })
}

json_header : { name : Str, value : Str }
json_header = { name: "content-type", value: "application/json" }

## PURE handler. Receives an already-reserved id and the decoded body — raw
## values, nothing to destructure; returns a 201 whose effects INSERT the row
## transactionally. A duplicate name fails the Must, so the host invokes the
## boxed err_handler (which captures `id`) and returns its 409 instead — and
## the transaction rolls back, so no row with that id survives.
create_user : I64, { name : Str } -> Response
create_user = |id, body| {
    name = body.name
    on_error = |err| match err {
        UniqueViolation(_) => {
            status: 409,
            headers: [json_header],
            body: "{\"error\":\"name taken\",\"rolled_back_id\":${id.to_str()}}".to_utf8(),
        }
        _ => {
            status: 500,
            headers: [json_header],
            body: "{\"error\":\"insert failed\"}".to_utf8(),
        }
    }
    Res.fx_err(
            DbWrite({ db: db_path, sql: "INSERT INTO users (id, name) VALUES (?, ?)", params: [Integer(id), Text(name)] }),
            on_error,
        )
        .opt(Log("created user ${id.to_str()} (${name})"))
        .json(201, { id: id, name: name })
}

## PURE handler: the `user_from!("id")` coeffect already loaded (or 404'd) the row.
get_user : { id : I64, name : Str } -> Response
get_user = |user| Res.json(200, user)

## PURE handler proving single-reservation through the record builder: both
## fields resolve to the SAME memoized id (a == b), consuming exactly one
## counter increment.
reserve_twice : { a : I64, b : I64 } -> Response
reserve_twice = |ids| Res.text(200, "a=${ids.a.to_str()};b=${ids.b.to_str()}")
