app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Ctx

# Reference PURE app: coeffects in, a pure handler that returns a Response
# carrying host-run effects. No handler calls Host.*! directly — reads are
# coeffects (resolved before the handler), writes are effects (run after).
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
# run. `ReservedId` reads this path from the model; the handlers use the same
# constant when they construct their DbWrite effects.
db_path : Str
db_path = "/tmp/tower-platform-pure-e2e.db"

## An app-defined READ coeffect: load the user named by the {id} path param.
## Its `inject!` runs before the handler; a miss rejects with 404 so the pure
## handler only ever sees a present user.
FetchedUser := [FetchedUser({ id : I64, name : Str })].{
    inject! = |req| {
        db = Box.unbox(req.model).db
        id_str = match Req.param(req, "id") {
            Ok(value) => value
            Err(_) => ""
        }
        match Host.sqlite_exec!({ db: db, sql: "SELECT id, name FROM users WHERE id = CAST(? AS INTEGER)", params: [Host.SqlValue.Text(id_str)] }) {
            Ok(result) => match FetchedUser.read_row(result) {
                Ok(user) => Ok(FetchedUser.FetchedUser(user))
                Err(_) => Err(Res.text(404, "user ${id_str} not found"))
            }
            Err(message) => Err(Res.text(500, "db error: ${message}"))
        }
    }

    read_row : { columns : List(Str), rows : List(List(Host.SqlValue)) } -> Try({ id : I64, name : Str }, [NotFound])
    read_row = |result| {
        match result.rows.first() {
            Ok(row) => {
                id = match row.get(0) {
                    Ok(cell) => match cell {
                        Host.SqlValue.Integer(n) => n
                        _ => -1
                    }
                    Err(_) => -1
                }
                name = match row.get(1) {
                    Ok(cell) => match cell {
                        Host.SqlValue.Text(s) => s
                        _ => ""
                    }
                    Err(_) => ""
                }
                Ok({ id: id, name: name })
            }
            Err(_) => Err(NotFound)
        }
    }

    value : FetchedUser -> { id : I64, name : Str }
    value = |wrapped| match wrapped { FetchedUser.FetchedUser(user) => user }
}

main = |{}| {
    settings: [
        CorsPermissive,
        TimeoutMs(10000),
        MaxBodyBytes(4096),
    ],
    init!: init!,
    routes: [
        # POST /users: reserve an id (non-idempotent coeffect) + decode the
        # JSON body, then a pure handler returns a Must(DbWrite) with a
        # UniqueViolation -> 409 err_handler and an Opt(Log).
        { method: "POST", path: "/users", handler: Box.box(Ctx.args2(create_user!)) },
        # GET /users/{id}: a read coeffect loads the row (404 on miss).
        { method: "GET", path: "/users/{id}", handler: Box.box(Ctx.args1(get_user!)) },
        # Two ReservedId slots in one request share ONE reservation (memoized).
        { method: "GET", path: "/reserve-twice", handler: Box.box(Ctx.args2(reserve_twice!)) },
    ],
}

init! : () => Try(Model, Str)
init! = || {
    _ = Host.sqlite_exec!({ db: db_path, sql: "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)", params: [] })
    Ok({ db: db_path })
}

json_header : { name : Str, value : Str }
json_header = { name: "content-type", value: "application/json" }

## PURE handler. Receives an already-reserved id and the decoded body; returns
## a 201 whose effects INSERT the row transactionally. A duplicate name fails
## the Must, so the host invokes the boxed err_handler (which captures `id`)
## and returns its 409 instead — and the transaction rolls back, so no row
## with that id survives.
create_user! : Req.ReservedId, Req.JsonBody({ name : Str }) -> Response
create_user! = |Req.ReservedId.ReservedId(id), Req.JsonBody.JsonBody(body)| {
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
    created = Res.json(201, { id: id, name: name })
    with_write = Res.with_effect(
        created,
        Res.must(
            DbWrite({ db: db_path, sql: "INSERT INTO users (id, name) VALUES (?, ?)", params: [Host.SqlValue.Integer(id), Host.SqlValue.Text(name)] }),
            on_error,
        ),
    )
    Res.with_effect(with_write, Res.opt(Log("created user ${id.to_str()} (${name})")))
}

## PURE handler: the FetchedUser coeffect already loaded (or 404'd) the row.
get_user! : FetchedUser -> Response
get_user! = |FetchedUser.FetchedUser(user)| Res.json(200, { id: user.id, name: user.name })

## PURE handler proving single-reservation: both ReservedId slots resolve to
## the SAME memoized id (a == b), consuming exactly one counter increment.
reserve_twice! : Req.ReservedId, Req.ReservedId -> Response
reserve_twice! = |Req.ReservedId.ReservedId(a), Req.ReservedId.ReservedId(b)|
    Res.text(200, "a=${a.to_str()};b=${b.to_str()}")
