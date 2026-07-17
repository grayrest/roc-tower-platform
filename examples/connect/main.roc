app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Sql
import pf.Cofx
import Connect

db_path = "/tmp/tower-platform-connect.db"

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

NewBook : { title : Str, author : Str, year : I64 }

## The route table wrapper. The mapping of errors to error responses is app-specific.
## Only one error handling strategy for this app so partial application makes for less
## noise in the route definition.
route = |method, path, handler| Req.route(method, path, handler, render)

## Render a coeffect/handler error to its Connect wire response. Coeffects
## propagate UNinteresting errors with bare postfix `?`, so the union is open —
## the wildcard maps anything that isn't a deliberate domain tag (`DbErr`,
## `NoColumn`, ...) to an opaque internal error, keeping engine strings off
## the wire. `InvalidBody` is the platform body extractor's rejection: its
## decode errors are formatted once, here, via `Req.explain` — no per-route
## hints. (Lives here, not in Connect.roc: calling a platform function from
## an app NOMINAL module crashes executable finalization — see the note.)
render = |error| match error {
    InvalidArgument(message) => Connect.error(InvalidArgument, message)
    NotFound(message) => Connect.error(NotFound, message)
    Internal(message) => Connect.error(Internal, message)
    InvalidBody(errors) => Connect.error(InvalidArgument, Req.explain(errors))
    _ => Connect.error(Internal, "internal error")
}

main = |{}| {
    settings: [
        TimeoutMs(10000),
        MaxBodyBytes(65536),
        ResponseCache({ ttl_ms: 30000, max_entries: 4096, methods: ["POST"], bypass_headers: [] }),
    ],
    init!: init!,
    routes: [
        route("POST", "/tower.connect.v1.CatalogService/GetBook", get_book_h!),
        route("POST", "/tower.connect.v1.CatalogService/ListBooks", list_books_h!),
        route("POST", "/tower.connect.v1.CatalogService/AddBook", add_book_h!),
        route("POST", "/tower.connect.v1.CatalogService/DeleteBook", delete_book_h!),
    ],
}

init! : () => Try(Model, Str)
init! = || {
    exec! = |sql| match Sql.migrate!(db_path, sql, []) {
        Ok(_) => Ok({})
        Err(DbErr(message)) => Err("schema: ${message}")
    }
    exec!("CREATE TABLE IF NOT EXISTS books (id INTEGER PRIMARY KEY, title TEXT NOT NULL, author TEXT NOT NULL, year INTEGER NOT NULL)")?
    Ok({ db: db_path })
}

# --- coeffects ---------------------------------------------------------------

## Coeffect: GetBook's target book — decode `{ id }` from the request message,
## load and decode its row. The body decode propagates bare: an invalid
## message rejects with the platform's `InvalidBody(errors)`, which `render`
## formats via `Req.explain` — no hand-written hint.
book! = |req| {
    request : { id : I64 }
    request = Req.json_body!(req)?
    rows = Sql.query!(req.ctx, db_path, "SELECT id, title, author, year FROM books WHERE id = ?", [Integer(request.id)])?
    row = Sql.first(rows) ? |_| NotFound("book ${request.id.to_str()} not found")
    book = decode_book(row)?
    Ok(book)
}

## Coeffect: every book, ordered by id.
books! = |req| {
    rows = Sql.query!(req.ctx, db_path, "SELECT id, title, author, year FROM books ORDER BY id", [])?
    books = decode_books(rows)?
    Ok(books)
}

# --- pure handlers -----------------------------------------------------------
# The write/answer side: pure functions from resolved coeffect values to a
# response. They never see the request — and may only CONSTRUCT effects; the
# host runs the returned effect list.

get_book_h! = Cofx.bind1(book!, get_book)

get_book : Book -> Response
get_book = |book| Res.json(200, {book: book})

list_books_h! = Cofx.bind1(books!, list_books)

list_books : List(Book) -> Response
list_books = |books| Res.json(200, {books: books})

## AddBook binds the platform body extractor DIRECTLY — no wrap coeffect. The
## handler's `NewBook` annotation IS the decode target; an invalid message
## rejects with `InvalidBody(errors)` and renders through `Req.explain`.
add_book_h! = Cofx.bind2(Req.json_body!, Req.reserved_id!, add_book)
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

## DeleteBook also binds the extractor directly; the annotation pins the
## message shape and the handler projects the field it wants.
delete_book_h! = Cofx.bind1(Req.json_body!, delete_book)

delete_book : { id : I64 } -> Response
delete_book = |request| {
    Res.fx(
        DbWrite({
            db: db_path,
            sql: "DELETE FROM books WHERE id = ?",
            params: [Integer(request.id)],
        }),
    )
    .json(200, {})
}

# --- module-private helpers --------------------------------------------------

decode_book : Sql.Row -> Try(Book, [NoColumn(Str), TypeMismatch(Str), ..])
decode_book = |row| Ok({
    id: row.i64("id")?,
    title: row.str("title")?,
    author: row.str("author")?,
    year: row.i64("year")?,
})

## Decode every row, short-circuiting on the first failure. Folds over a `Try`
## accumulator — a form that used to crash the compiler's codegen but builds
## since the 2026-07-16 upstream update (see the compiler-bug note).
decode_books : List(Sql.Row) -> Try(List(Book), [NoColumn(Str), TypeMismatch(Str), ..])
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
