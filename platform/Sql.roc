## Typed access to SQLite results over the streaming cursor FFI.
##
## `Sql.query!` opens a cursor (`Host.sql_begin!`), steps every row into a
## by-name-decodable `Sql.Row`, and ALWAYS finishes the cursor before
## returning — the handle never escapes this call (plan decision 2), so a `?`
## in a caller can't leak it. Reads decode cells BY NAME into typed values:
##
##     rows = Sql.query!(req.ctx, db, sql, params)?
##     row = Sql.first(rows) ? |_| NotFound
##     id = row.i64("id")?
##     name = row.str("name")?
##     bio = row.nullable_str("bio")?
##
## The `Row` decoders are pure `->` functions (expect-testable); only the
## acquisition (`query!`/`exec!`) is effectful. This is the dynamic/hand-written
## layer — generated readers (sqlgen) skip the `Sql.Row` materialization and
## read cells positionally straight into records.
Sql := [].{
    ## Decode failure: the named column is absent from the result, or the
    ## cell doesn't hold the requested type.
    Err : [NoColumn(Str), TypeMismatch(Str)]

    ## One result row paired with the result's column names, so cells can be
    ## fetched by name instead of position.
    Row := { columns : List(Str), cells : List(Host.SqlValue) }.{
        ## The raw cell under a column name.
        cell : Sql.Row, Str -> Try(Host.SqlValue, Sql.Err)
        cell = |row, name| {
            index = row.columns.find_first_index(|column| column == name) ? |_| NoColumn(name)
            value = row.cells.get(index) ? |_| NoColumn(name)
            Ok(value)
        }

        ## A TEXT cell as Str.
        str : Sql.Row, Str -> Try(Str, Sql.Err)
        str = |row, name| {
            value = Sql.Row.cell(row, name)?
            match value {
                Text(text) => Ok(text)
                _ => Err(TypeMismatch(name))
            }
        }

        ## An INTEGER cell as I64.
        i64 : Sql.Row, Str -> Try(I64, Sql.Err)
        i64 = |row, name| {
            value = Sql.Row.cell(row, name)?
            match value {
                Integer(number) => Ok(number)
                _ => Err(TypeMismatch(name))
            }
        }

        ## A REAL cell as F64.
        f64 : Sql.Row, Str -> Try(F64, Sql.Err)
        f64 = |row, name| {
            value = Sql.Row.cell(row, name)?
            match value {
                Real(number) => Ok(number)
                _ => Err(TypeMismatch(name))
            }
        }

        ## An INTEGER cell as Bool (SQLite booleans: 0 is False, anything
        ## else True) — the shape of EXISTS / COUNT flag columns.
        bool : Sql.Row, Str -> Try(Bool, Sql.Err)
        bool = |row, name| {
            value = Sql.Row.cell(row, name)?
            match value {
                Integer(number) => Ok(number != 0)
                _ => Err(TypeMismatch(name))
            }
        }

        ## A nullable TEXT cell: NULL becomes `NullableStr.Null`, TEXT
        ## becomes `Val`, ready for JSON-null output.
        nullable_str : Sql.Row, Str -> Try(NullableStr, Sql.Err)
        nullable_str = |row, name| {
            value = Sql.Row.cell(row, name)?
            match value {
                Null => Ok(NullableStr.Null)
                Text(text) => Ok(NullableStr.Val(text))
                _ => Err(TypeMismatch(name))
            }
        }
    }

    ## Run a query and materialize every row for by-name decode. Opens a
    ## cursor under `ctx` (the request coeffect ctx, 0 at init time) against
    ## `db`, steps all rows, and finishes the cursor on every path.
    query! : U64, Str, Str, List(Host.SqlValue) => Try(List(Sql.Row), Str)
    query! = |ctx, db, sql, params| {
        handle = Host.sql_begin!({ ctx: ctx, db: db, sql: sql, params: params })?
        columns = Host.sql_columns!(handle)
        collected = collect_rows!(handle, columns, [])
        Host.sql_finish!(handle)
        collected
    }

    ## Read the first row's first column as a preformed JSON document — the raw
    ## path mirroring an `@sql json` generated reader, for dynamically-built
    ## queries that assemble their JSON in SQL (`json_object`/
    ## `json_group_array`). No `Sql.Row` materialization, no re-encode: the one
    ## cell is copied straight into a `RawJson`. Finishes the cursor on every
    ## arm; errors if the query yields no row.
    query_json! : U64, Str, Str, List(Host.SqlValue) => Try(RawJson, Str)
    query_json! = |ctx, db, sql, params| {
        handle = Host.sql_begin!({ ctx: ctx, db: db, sql: sql, params: params })?
        result = read_first_json!(handle)
        Host.sql_finish!(handle)
        result
    }

    ## Run a statement for effect only (DDL, or a write with no rows worth
    ## reading), draining any result rows. Finishes the cursor on every path.
    exec! : U64, Str, Str, List(Host.SqlValue) => Try({}, Str)
    exec! = |ctx, db, sql, params| {
        handle = Host.sql_begin!({ ctx: ctx, db: db, sql: sql, params: params })?
        drained = drain_rows!(handle)
        Host.sql_finish!(handle)
        drained
    }

    ## The first row of a materialized result (the single-row-query case:
    ## lookups by key, COUNT/EXISTS aggregates).
    first : List(Sql.Row) -> Try(Sql.Row, [NoRows])
    first = |rows| {
        row = rows.first() ? |_| NoRows
        Ok(row)
    }
}

import Host
import NullableStr
import RawJson

# --- module-private effectful cursor helpers ---
#
# Top-level defs outside the nominal block are unreachable from other modules,
# so these stepping loops are the platform's real privacy boundary.

## Step once and read column 0 as a preformed JSON document; error if there is
## no row. Does NOT finish the cursor — `query_json!` owns that on every arm.
read_first_json! : U64 => Try(RawJson, Str)
read_first_json! = |handle| {
    has_row = Host.sql_step!(handle)?
    if has_row {
        Ok(RawJson.from(Host.sql_col_str!(handle, 0)))
    } else {
        Err("query_json! returned no rows")
    }
}

## Step every remaining row into `acc`, decoding each cell by its live storage
## class. Returns the accumulated rows, or the first step error. Does NOT
## finish the cursor — `query!` owns that on every path.
collect_rows! : U64, List(Str), List(Sql.Row) => Try(List(Sql.Row), Str)
collect_rows! = |handle, columns, acc| {
    match Host.sql_step!(handle) {
        Err(message) => Err(message)
        Ok(has_row) =>
            if has_row {
                row = Sql.Row.{ columns: columns, cells: read_cells!(handle, 0, List.len(columns), []) }
                collect_rows!(handle, columns, acc.append(row))
            } else {
                Ok(acc)
            }
    }
}

## Read cells `[start, count)` of the current row into `acc`.
read_cells! : U64, U64, U64, List(Host.SqlValue) => List(Host.SqlValue)
read_cells! = |handle, index, count, acc| {
    if index >= count {
        acc
    } else {
        read_cells!(handle, index + 1, count, acc.append(read_cell!(handle, index)))
    }
}

## One cell of the current row, tagged by its live storage class.
read_cell! : U64, U64 => Host.SqlValue
read_cell! = |handle, index| {
    match Host.sql_col_type!(handle, index) {
        Null => Null
        Integer => Integer(Host.sql_col_i64!(handle, index))
        Real => Real(Host.sql_col_f64!(handle, index))
        Text => Text(Host.sql_col_str!(handle, index))
        Blob => Blob(Host.sql_col_blob!(handle, index))
    }
}

## Step to exhaustion, discarding rows (for statements run only for effect).
drain_rows! : U64 => Try({}, Str)
drain_rows! = |handle| {
    match Host.sql_step!(handle) {
        Err(message) => Err(message)
        Ok(has_row) => if has_row { drain_rows!(handle) } else { Ok({}) }
    }
}

## str decodes a TEXT cell by column name
expect {
    row = Sql.Row.{ columns: ["id", "name"], cells: [Integer(7), Text("jake")] }
    row.str("name") == Ok("jake")
}

## str on a non-TEXT cell reports TypeMismatch with the column name
expect {
    row = Sql.Row.{ columns: ["id"], cells: [Integer(7)] }
    row.str("id") == Err(TypeMismatch("id"))
}

## a missing column reports NoColumn with the requested name
expect {
    row = Sql.Row.{ columns: ["id"], cells: [Integer(7)] }
    row.str("slug") == Err(NoColumn("slug"))
}

## i64 decodes an INTEGER cell by column name
expect {
    row = Sql.Row.{ columns: ["id", "name"], cells: [Integer(7), Text("jake")] }
    row.i64("id") == Ok(7)
}

## f64 decodes a REAL cell by column name
expect {
    row = Sql.Row.{ columns: ["score"], cells: [Real(1.5)] }
    row.f64("score") == Ok(1.5)
}

## bool reads SQLite flag columns: 0 is False
expect {
    row = Sql.Row.{ columns: ["favorited"], cells: [Integer(0)] }
    row.bool("favorited") == Ok(False)
}

## bool reads SQLite flag columns: nonzero is True
expect {
    row = Sql.Row.{ columns: ["favorited"], cells: [Integer(1)] }
    row.bool("favorited") == Ok(True)
}

## nullable_str turns a NULL cell into NullableStr.Null
expect {
    row = Sql.Row.{ columns: ["bio"], cells: [Null] }
    row.nullable_str("bio") == Ok(NullableStr.Null)
}

## nullable_str turns a TEXT cell into NullableStr.Val
expect {
    row = Sql.Row.{ columns: ["bio"], cells: [Text("hi")] }
    row.nullable_str("bio") == Ok(NullableStr.Val("hi"))
}

## first returns the first row of a non-empty result
expect {
    rows = [Sql.Row.{ columns: ["n"], cells: [Integer(1)] }, Sql.Row.{ columns: ["n"], cells: [Integer(2)] }]
    match Sql.first(rows) {
        Ok(row) => row.i64("n") == Ok(1)
        Err(_) => False
    }
}

## first on an empty result reports NoRows
expect {
    rows : List(Sql.Row)
    rows = []
    Sql.first(rows) == Err(NoRows)
}
