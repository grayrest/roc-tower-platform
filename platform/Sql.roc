## Decode helpers for `Host.sqlite_exec!` results. A result arrives as
## `{ columns : List(Str), rows : List(List(Host.SqlValue)) }`; matching
## positions by hand is verbose and breaks silently when a SELECT reorders.
## `Sql.rows` / `Sql.first` pair each row with the column names, and the
## `Row` methods decode cells BY NAME into typed values:
##
##     row = Sql.first(result) ? |_| NotFound
##     id = row.i64("id")?
##     name = row.str("name")?
##     bio = row.nullable_str("bio")?
##
## Every decode returns `Try` with the offending column name in the error,
## so `?` / `? |_| ...` chains keep handlers flat.
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

    ## Pair every result row with the column names for named-cell access.
    rows : { columns : List(Str), rows : List(List(Host.SqlValue)) } -> List(Sql.Row)
    rows = |result| result.rows.map(|cells| Sql.Row.{ columns: result.columns, cells: cells })

    ## The first row of a result (the single-row-query case: lookups by key,
    ## COUNT/EXISTS aggregates).
    first : { columns : List(Str), rows : List(List(Host.SqlValue)) } -> Try(Sql.Row, [NoRows])
    first = |result| {
        cells = result.rows.first() ? |_| NoRows
        Ok(Sql.Row.{ columns: result.columns, cells: cells })
    }
}

import Host
import NullableStr

## rows pairs each row's cells with the result's column names
expect {
    result = { columns: ["id"], rows: [[Integer(1)], [Integer(2)]] }
    paired = Sql.rows(result)
    paired.len() == 2
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
    result = { columns: ["n"], rows: [[Integer(1)], [Integer(2)]] }
    match Sql.first(result) {
        Ok(row) => row.i64("n") == Ok(1)
        Err(_) => False
    }
}

## first on an empty result reports NoRows
expect {
    result : { columns : List(Str), rows : List(List(Host.SqlValue)) }
    result = { columns: ["n"], rows: [] }
    Sql.first(result) == Err(NoRows)
}
