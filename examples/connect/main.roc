app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Sql
import pf.Cofx
import Connect

# A Connect-protocol RPC service (unary, JSON) on the tower-platform —  Every method is `POST
# /{service}/{Method}` with the request message as the JSON body and the response message as the
# JSON body; there are no path params, no REST verbs, and errors are Connect envelopes, not bare
# statuses.
#
# The interesting property: read-only methods are cached automatically. A query
# is a POST whose body IS the request message, so the cache keys on the body
# hash — `GetBook` fragments per id, `ListBooks` takes no input so every
# call shares one entry. A mutation (`AddBook`/`DeleteBook`) emits a `DbWrite`,
# which poisons its own response (never cached) and purges every cached read of
# the `books` table it touched.
Model : { db : Str }

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

Book : { id : I64, title : Str, author : Str, year : I64 }

db_path : Str
db_path = "/tmp/tower-platform-connect.db"

main = |{}| {
    settings: [
        TimeoutMs(10000),
        MaxBodyBytes(65536),
        # Connect uses POST for every method, so the cache must include POST
        # (and ttl_ms must be > 0 — the host rejects 0 at startup).
        ResponseCache({ ttl_ms: 30000, max_entries: 4096, methods: ["POST"], bypass_headers: [] }),
    ],
    init!: init!,
    routes: [
        route("POST", "/tower.connect.v1.CatalogService/GetBook", bind1(book!, get_book)),
        route("POST", "/tower.connect.v1.CatalogService/ListBooks", bind1(books!, list_books)),
        route("POST", "/tower.connect.v1.CatalogService/AddBook", bind2(fields!, new_id!, add_book)),
        route("POST", "/tower.connect.v1.CatalogService/DeleteBook", bind1(target_id!, delete_book)),
    ],
}

## The route table wrapper. The mapping of errors to error responses is app-specific.
## This app uses Connect protocol JSON for it. Since most apps only have one error
## mapping strategy, partially applying it makes route definition less noisy.
route = |method, path, handler| Req.route(method, path, handler, Connect.render)

## Bind domain-fallible coeffects to a PURE handler: `Cofx.withN` with each
## rejection mapped into `Err(domain error)` and the handler's response into
## `Ok`, yielding the fallible `Request => Try(Response, Connect.Error)` shape
## `route` renders. Coeffects keep the bare `?`-over-domain-error style and
## pure handlers return a plain `Response`; the adaptation lives here once.
bind1 = |resolver!, handler|
    Cofx.with1(Cofx.map_reject(resolver!, |e| Err(e)), |a| Ok(handler(a)))

bind2 = |resolver_a!, resolver_b!, handler|
    Cofx.with2(Cofx.map_reject(resolver_a!, |e| Err(e)), Cofx.map_reject(resolver_b!, |e| Err(e)), |a, b| Ok(handler(a, b)))

init! : () => Try(Model, Str)
init! = || {
    exec! = |sql| match Sql.migrate!(db_path, sql, []) {
        Ok(_) => Ok({})
        Err(message) => Err("schema: ${message}")
    }
    exec!("CREATE TABLE IF NOT EXISTS books (id INTEGER PRIMARY KEY, title TEXT NOT NULL, author TEXT NOT NULL, year INTEGER NOT NULL)")?
    Ok({ db: db_path })
}

# --- coeffects ---------------------------------------------------------------
# The read side of each RPC: a coeffect is a plain function `Request =>
# Try(value, Connect.Error)` that either produces a handler input (decode the
# request message, load rows) or rejects with a domain error via infix `?` —
# `? |_| NotFound("...")` for a literal message, a bare constructor like
# `? Internal` to wrap the underlying error. Reads flow through `Sql.query!`
# (recorded for cache invalidation); decoding the body marks the body-variant
# cache key.

## Coeffect: GetBook's target book — decode `{ id }` from the request message,
## load and decode its row.
book! = |req| {
    request : { id : I64 }
    request = Req.json_body!(req) ? |_| InvalidArgument("GetBook expects { \"id\": <integer> }")
    rows = Sql.query!(req.ctx, db_path, "SELECT id, title, author, year FROM books WHERE id = ?", [Integer(request.id)]) ? Internal
    row = Sql.first(rows) ? |_| NotFound("book ${request.id.to_str()} not found")
    book = decode_book(row) ? |_| Internal("corrupt book row")
    Ok(book)
}

## Coeffect: every book, ordered by id.
books! = |req| {
    rows = Sql.query!(req.ctx, db_path, "SELECT id, title, author, year FROM books ORDER BY id", []) ? Internal
    books = decode_books(rows) ? |_| Internal("corrupt book row")
    Ok(books)
}

## Coeffect: AddBook's request message.
fields! = |req| {
    fields : { title : Str, author : Str, year : I64 }
    fields = Req.json_body!(req) ? |_| InvalidArgument("AddBook expects { title, author, year }")
    Ok(fields)
}

## Coeffect: a freshly reserved book id (memoized — every slot in one request
## shares the same reservation).
new_id! = |req| {
    id = Req.reserved_id!(req) ? |_| Internal("id reservation failed")
    Ok(id)
}

## Coeffect: DeleteBook's target id from the request message.
target_id! = |req| {
    request : { id : I64 }
    request = Req.json_body!(req) ? |_| InvalidArgument("DeleteBook expects { \"id\": <integer> }")
    Ok(request.id)
}

# --- pure handlers -----------------------------------------------------------
# The write/answer side: pure functions from resolved coeffect values to a
# response. They never see the request — and may only CONSTRUCT effects; the
# host runs the returned effect list.

## GetBook({ id }) -> { book }. Read-only, so cacheable — keyed on the request
## body, so each id is a distinct cache entry.
get_book = |book| {
    # Pin the argument locally (the handler is unannotated), so `Res.json`
    # can resolve a JSON encoder for the response record.
    target : Book
    target = book
    Res.json(200, { book: target })
}

## ListBooks({}) -> { books }. Read-only, so cacheable. It never reads the
## request body, so it doesn't vary on it — every call shares one entry.
list_books = |books| {
    all : List(Book)
    all = books
    Res.json(200, { books: all })
}

## AddBook({ title, author, year }) -> { book }. A DbWrite mutation: the effect
## poisons this response (never cached) and purges every cached read of `books`.
add_book = |fields, id| {
    book : Book
    book = { id: id, title: fields.title, author: fields.author, year: fields.year }
    Res.fx(
        DbWrite({
            db: db_path,
            sql: "INSERT INTO books (id, title, author, year) VALUES (?, ?, ?, ?)",
            params: [
                Integer(id),
                Text(fields.title),
                Text(fields.author),
                Integer(fields.year),
            ],
        }),
    )
    .json(200, { book: book })
}

## DeleteBook({ id }) -> {}. A DbWrite mutation; purges cached reads of `books`.
delete_book = |id| {
    Res.fx(
        DbWrite({
            db: db_path,
            sql: "DELETE FROM books WHERE id = ?",
            params: [Integer(id)],
        }),
    )
    .json(200, {})
}

# --- module-private helpers --------------------------------------------------

decode_book : Sql.Row -> Try(Book, Sql.Err)
decode_book = |row| Ok({
    id: row.i64("id")?,
    title: row.str("title")?,
    author: row.str("author")?,
    year: row.i64("year")?,
})

## Decode every row, short-circuiting on the first failure. Folds over a `Try`
## accumulator — a form that used to crash the compiler's codegen but builds
## since the 2026-07-16 upstream update (see the compiler-bug note).
decode_books : List(Sql.Row) -> Try(List(Book), Sql.Err)
decode_books = |rows|
    rows.fold(Ok([]), |acc, row|
        match acc {
            Err(error) => Err(error)
            Ok(books) =>
                match decode_book(row) {
                    Ok(book) => Ok(books.append(book))
                    Err(error) => Err(error)
                }
        }
    )
