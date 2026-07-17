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

## The route table wrapper. The mapping of errors to error responses is app-specific.
## Only one error handling strategy for this app so partial application makes for less
## noise in the route definition.
route = |method, path, handler| Req.route(method, path, handler, Connect.render)

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
# The read side of each RPC: a coeffect is a plain function `Request =>
# Try(value, err)` that either produces a handler input (decode the request
# message, load rows) or rejects. INTERESTING errors are raised as domain tags
# (`? |_| NotFound("...")`); uninteresting ones just propagate with bare
# postfix `?` — the platform's errors are open tag unions (`DbErr`, `NoColumn`,
# `TypeMismatch`, ...), so they merge into the coeffect's inferred union and
# fall to `Connect.render`'s wildcard (500). Reads flow through `Sql.query!`
# (recorded for cache invalidation); decoding the body marks the body-variant
# cache key.

## Coeffect: GetBook's target book — decode `{ id }` from the request message,
## load and decode its row.
book! = |req| {
    request : { id : I64 }
    request = Req.json_body!(req) ? |_| InvalidArgument("GetBook expects { \"id\": <integer> }")
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

NewBook : { title : Str, author : Str, year : I64 }

## Coeffect: AddBook's request message.
new_book! = |req| {
    fields : NewBook
    fields = Req.json_body!(req) ? |_| InvalidArgument("Book creation expects { title, author, year }")
    Ok(fields)
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

get_book_h! = Cofx.bind1(book!, get_book)

get_book : Book -> Response
get_book = |book| Res.json(200, {book: book})

list_books_h! = Cofx.bind1(books!, list_books)

list_books : List(Book) -> Response
list_books = |books| Res.json(200, {books: books})

add_book_h! = Cofx.bind2(new_book!, Req.reserved_id!, add_book)
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

delete_book_h! = Cofx.bind1(target_id!, delete_book)
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
