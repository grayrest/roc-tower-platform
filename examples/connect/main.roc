app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.Req
import pf.Sql
import Connect

# A Connect-protocol RPC service (unary, JSON) on the tower-platform — a
# deliberately different SHAPE from the REST realworld demo, over the SAME
# platform primitives (coeffect reads, DbWrite effects, the derived response
# cache). Every method is `POST /{service}/{Method}` with the request message
# as the JSON body and the response message as the JSON body; there are no path
# params, no REST verbs, and errors are Connect envelopes, not bare statuses.
#
# The interesting property: read-only methods are cached automatically. A query
# is a POST whose body IS the request message, so the cache keys on the body
# hash (D14) — `GetBook` fragments per id, `ListBooks` takes no input so every
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
        # Connect uses POST for every method, so the cache must include POST.
        # `bypass_headers` is left empty — the platform hard-defaults
        # `authorization`, and this demo is unauthenticated anyway.
        ResponseCache({ ttl_ms: 30000, max_entries: 4096, methods: ["POST"], bypass_headers: [] }),
    ],
    init!: init!,
    routes: [
        { method: "POST", path: "/tower.connect.v1.CatalogService/GetBook", handler: Box.box(get_book!) },
        { method: "POST", path: "/tower.connect.v1.CatalogService/ListBooks", handler: Box.box(list_books!) },
        { method: "POST", path: "/tower.connect.v1.CatalogService/AddBook", handler: Box.box(add_book!) },
        { method: "POST", path: "/tower.connect.v1.CatalogService/DeleteBook", handler: Box.box(delete_book!) },
    ],
}

init! : () => Try(Model, Str)
init! = || {
    exec! = |sql| match Sql.migrate!(db_path, sql, []) {
        Ok(_) => Ok({})
        Err(message) => Err("schema: ${message}")
    }
    exec!("CREATE TABLE IF NOT EXISTS books (id INTEGER PRIMARY KEY, title TEXT NOT NULL, author TEXT NOT NULL, year INTEGER NOT NULL)")?
    Ok({ db: db_path })
}

# --- unary RPC handlers ------------------------------------------------------
# Raw `Request => Response` handlers: decode the request message, read or write,
# return a Connect success or error. Reads flow through `Sql.query!` (recorded
# for cache invalidation); decoding the body marks the body-variant cache key;
# mutations return `DbWrite` effects the host runs after the handler.

## GetBook({ id }) -> { book }. Read-only, so cacheable — keyed on the request
## body, so each id is a distinct cache entry.
get_book! : Request => Response
get_book! = |req| {
    parsed : Try({ id : I64 }, _)
    parsed = Req.json_body!(req)
    match parsed {
        Err(_) => Connect.error(InvalidArgument, "GetBook expects { \"id\": <integer> }")
        Ok(request) =>
            match Sql.query!(req.ctx, db_path, "SELECT id, title, author, year FROM books WHERE id = ?", [Host.SqlValue.Integer(request.id)]) {
                Err(message) => Connect.error(Internal, message)
                Ok(rows) =>
                    match Sql.first(rows) {
                        Err(_) => Connect.error(NotFound, "book ${request.id.to_str()} not found")
                        Ok(row) =>
                            match decode_book(row) {
                                Ok(book) => Res.json(200, { book: book })
                                Err(_) => Connect.error(Internal, "corrupt book row")
                            }
                    }
            }
    }
}

## ListBooks({}) -> { books }. Read-only, so cacheable. It never reads the
## request body, so it doesn't vary on it — every call shares one entry.
list_books! : Request => Response
list_books! = |req|
    match Sql.query!(req.ctx, db_path, "SELECT id, title, author, year FROM books ORDER BY id", []) {
        Err(message) => Connect.error(Internal, message)
        Ok(rows) =>
            match decode_books(rows) {
                Ok(books) => Res.json(200, { books: books })
                Err(_) => Connect.error(Internal, "corrupt book row")
            }
    }

## AddBook({ title, author, year }) -> { book }. A DbWrite mutation: the effect
## poisons this response (never cached) and purges every cached read of `books`.
add_book! : Request => Response
add_book! = |req| {
    parsed : Try({ title : Str, author : Str, year : I64 }, _)
    parsed = Req.json_body!(req)
    match parsed {
        Err(_) => Connect.error(InvalidArgument, "AddBook expects { title, author, year }")
        Ok(fields) =>
            match Req.reserved_id!(req) {
                Err(response) => response
                Ok(id) => {
                    book : Book
                    book = { id: id, title: fields.title, author: fields.author, year: fields.year }
                    Res.fx(
                        DbWrite({
                            db: db_path,
                            sql: "INSERT INTO books (id, title, author, year) VALUES (?, ?, ?, ?)",
                            params: [
                                Host.SqlValue.Integer(id),
                                Host.SqlValue.Text(fields.title),
                                Host.SqlValue.Text(fields.author),
                                Host.SqlValue.Integer(fields.year),
                            ],
                        }),
                    )
                    .json(200, { book: book })
                }
            }
    }
}

## DeleteBook({ id }) -> {}. A DbWrite mutation; purges cached reads of `books`.
delete_book! : Request => Response
delete_book! = |req| {
    parsed : Try({ id : I64 }, _)
    parsed = Req.json_body!(req)
    match parsed {
        Err(_) => Connect.error(InvalidArgument, "DeleteBook expects { \"id\": <integer> }")
        Ok(request) =>
            Res.fx(
                DbWrite({
                    db: db_path,
                    sql: "DELETE FROM books WHERE id = ?",
                    params: [Host.SqlValue.Integer(request.id)],
                }),
            )
            .json(200, {})
    }
}

# --- module-private helpers --------------------------------------------------

decode_book : Sql.Row -> Try(Book, Sql.Err)
decode_book = |row| Ok({
    id: row.i64("id")?,
    title: row.str("title")?,
    author: row.str("author")?,
    year: row.i64("year")?,
})

## Decode every row, short-circuiting on the first failure. Recurses by index
## (the pattern `Sql.roc` uses) rather than folding over a `Try` accumulator,
## which crashes the compiler's codegen.
decode_books : List(Sql.Row) -> Try(List(Book), Sql.Err)
decode_books = |rows| decode_books_from(rows, 0, [])

decode_books_from : List(Sql.Row), U64, List(Book) -> Try(List(Book), Sql.Err)
decode_books_from = |rows, index, acc|
    if index >= List.len(rows) {
        Ok(acc)
    } else {
        match rows.get(index) {
            Err(_) => Ok(acc)
            Ok(row) =>
                match decode_book(row) {
                    Ok(book) => decode_books_from(rows, index + 1, acc.append(book))
                    Err(error) => Err(error)
                }
        }
    }
