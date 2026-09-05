# Standalone smoke test + load harness for H3: the internal-iteration cursor
# fold with BORROWED-SLICE rows.
#
# `GET /rows` opens a cursor over a small seeded table and folds every row into
# a JSON array-of-arrays with `Host.sql_fold_bytes!`. The host builds each row as
# a `List(SqlValue)` whose Text cells BORROW the live SQLite bytes (zero copy,
# immortal to Roc); the reducer below is ordinary PURE Roc that serializes each
# cell through `JsonEnc` — no pure-typed host cell reads and no behind-the-glue
# list growth (the two hacks H1/H2 traded away). See README.md.
#
# The seed data exercises every SqlValue arm the reducer matches: Integer, a
# Text cell needing JSON escaping (embedded quotes and a newline), and NULL.
app [Model, main] { pf: platform "../../platform/main.roc" }

import pf.Host
import pf.Res
import pf.JsonEnc

Model : { db : Str }

db_path = "/tmp/tower-slice-spike.db"

Request : {
    ctx : U64,
    method : Str,
    path : Str,
    params : List({ name : Str, value : Str }),
    query : Str,
    headers : List({ name : Str, value : Str }),
    model : Box(Model),
}

main = |{}| {
    settings: [TimeoutMs(10000)],
    init!: init!,
    routes: [{ method: "GET", path: "/rows", handler: Box.box(rows!) }],
}

## Create and seed the table (mixed types, escaping, a NULL).
init! : () => Try(Model, Str)
init! = || {
    exec!("CREATE TABLE IF NOT EXISTS t (id INTEGER, name TEXT, bio TEXT, cnt INTEGER)")?
    exec!("DELETE FROM t")?
    exec!(
        \\INSERT INTO t (id, name, bio, cnt) VALUES
        \\  (1, 'jake', 'the "real" jake', 42),
        \\  (2, 'bob', NULL, 7),
        \\  (3, 'sy', 'tab	and newline
        \\here', 0)
    )?
    Ok({ db: db_path })
}

## Run a write statement to completion: begin, one step to execute, finish.
exec! : Str => Try({}, Str)
exec! = |sql| {
    handle = Host.sql_begin!({ ctx: 0, db: db_path, sql, params: [] })?
    _ = Host.sql_step!(handle)?
    Host.sql_finish!(handle)
    Ok({})
}

rows! : Request => Res.Response
rows! = |req| {
    db = Box.unbox(req.model).db
    match fold_rows!(db) {
        Ok(body) => Res.bytes(200, "application/json", body)
        Err(message) => Res.text(500, message)
    }
}

## Open a cursor and fold every row into a JSON array of arrays.
fold_rows! : Str => Try(List(U8), Str)
fold_rows! = |db| {
    handle = Host.sql_begin!({ ctx: 0, db, sql: "SELECT id, name, bio, cnt FROM t ORDER BY id", params: [] })?
    folded = Host.sql_fold_bytes!(handle, [], Box.box(reduce))
    Host.sql_finish!(handle)
    body = folded?
    Ok(close_array(body))
}

## Reduce one row into the accumulator as `[cell,cell,...],`. Each string cell
## is JSON-escaped through the ordinary `JsonEnc` append path over the BORROWED
## `Str` — the copy that escaping performs is the row's single copy.
reduce : List(U8), List(Host.SqlValue) -> List(U8)
reduce = |acc, cols| {
    opened = JsonEnc.raw_into(acc, "[")
    filled = List.fold(cols, { out: opened, first: True }, |st, cell| {
        separated = if st.first st.out else JsonEnc.raw_into(st.out, ",")
        { out: encode_cell(separated, cell), first: False }
    })
    JsonEnc.raw_into(filled.out, "],")
}

encode_cell : List(U8), Host.SqlValue -> List(U8)
encode_cell = |out, cell| match cell {
    Text(s) => JsonEnc.str_into(out, s)
    Integer(n) => JsonEnc.i64_into(out, n)
    Real(_) => JsonEnc.raw_into(out, "0")
    Null => JsonEnc.null_into(out)
    Blob(_) => JsonEnc.str_into(out, "<blob>")
}

## Wrap the comma-joined rows in `[ ... ]`, dropping the one trailing comma.
## 91 = '[', 93 = ']', 44 = ','.
close_array : List(U8) -> List(U8)
close_array = |body| {
    trimmed = match List.last(body) {
        Ok(44) => List.drop_last(body, 1)
        _ => body
    }
    List.append(List.concat([91], trimmed), 93)
}
