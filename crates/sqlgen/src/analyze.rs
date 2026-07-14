//! VDBE-bytecode result-column analyzer.
//!
//! SQLite's prepared-statement `decl_type()` types every column that comes
//! directly from a table, but returns `None` for expression columns
//! (`COUNT(*)`, `EXISTS(...)`, `GROUP_CONCAT(...)`) and never reports
//! nullability. This module fills both gaps by walking the `EXPLAIN` bytecode:
//! it maps each result register back through `Copy`/`Move` chains to the
//! opcode that produced it, and classifies the root (a table `Column`, a rowid,
//! a literal, or an aggregate `AggFinal`) into a storage type + nullability.
//!
//! Design + kill criterion: `plans/2026-07-13-typed-sql.md` Phase 1. The
//! criterion is `article_select`: favorites_count / favorited / following must
//! come out `Integer NOT NULL`, tag_list `Text` nullable, plain columns typed
//! from the schema with bio/image nullable.

use rusqlite::Connection;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    Integer,
    Real,
    Text,
    Blob,
}

impl SqlType {
    /// Map a declared-type / affinity string to a storage class, per SQLite's
    /// affinity rules (<https://www.sqlite.org/datatype3.html> §3.1).
    fn from_decl(decl: &str) -> SqlType {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            SqlType::Integer
        } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
            SqlType::Text
        } else if d.contains("BLOB") || d.is_empty() {
            SqlType::Blob
        } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
            SqlType::Real
        } else {
            // NUMERIC affinity — default to Integer for our schema (no decimals).
            SqlType::Integer
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnType {
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// Placeholder name without the leading `:` (`:viewer` → `viewer`), or the
    /// positional index rendered as a string when unnamed.
    pub name: String,
    pub ty: Option<SqlType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub columns: Vec<ColumnType>,
    pub params: Vec<Param>,
    /// `sqlite3_stmt_readonly` — a read (SELECT / read-only CTE) vs a write.
    pub readonly: bool,
}

#[derive(Debug)]
struct Insn {
    addr: i64,
    opcode: String,
    p1: i64,
    p2: i64,
    p3: i64,
    p4: String,
}

/// What a VDBE cursor reads from: a table (colidx → table column) or an index
/// (colidx → indexed table column via index_info).
#[derive(Debug, Clone)]
enum Cursor {
    Table(String),
    /// (table name, index columns as table cids in index order)
    Index(String, Vec<i64>),
    /// An ephemeral / sorter / autoindex cursor — a materialized intermediate
    /// (RETURNING scratch, IN-list, ORDER BY sorter). Its columns can't be
    /// traced to a schema type, so they degrade to (Blob, nullable). Real
    /// table/index reads stay strict so a genuinely unknown cursor still errors.
    Ephemeral,
}

/// Cursor resolutions plus the set of cursors an outer join can NULL out
/// (targets of a `NullRow` opcode): columns read from those are nullable
/// regardless of the table's NOT NULL.
struct CursorMap {
    cursors: HashMap<i64, Cursor>,
    nullable_by_join: std::collections::HashSet<i64>,
}

pub struct Analyzer<'c> {
    conn: &'c Connection,
}

impl<'c> Analyzer<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Analyzer { conn }
    }

    /// Type every result column and parameter of `sql` (already schema-valid).
    pub fn analyze(&self, sql: &str) -> Result<Analysis, String> {
        let stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("prepare failed: {e}"))?;
        let readonly = stmt.readonly();
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let decltypes: Vec<Option<SqlType>> = stmt
            .columns()
            .iter()
            .map(|c| c.decl_type().map(SqlType::from_decl))
            .collect();
        let param_names = self.param_names(&stmt);
        drop(stmt);

        let program = self.explain(sql)?;
        let cursors = self.resolve_cursors(&program)?;

        // No ResultRow ⇒ a plain write (INSERT/UPDATE/DELETE, no RETURNING):
        // no result columns, and that's correct, not an error.
        let mut columns = Vec::new();
        if let Some(result_row) = program.iter().find(|i| i.opcode == "ResultRow") {
            let base = result_row.p1;
            let count = result_row.p2;
            let use_addr = result_row.addr;
            for i in 0..count {
                let reg = base + i;
                let (ty, nullable) = self.type_register(&program, &cursors, reg, use_addr)?;
                // Prefer decltype for the base type where SQLite gave one (it is
                // authoritative for table-sourced columns); fall back to the VDBE
                // root for expression columns.
                let ty = decltypes.get(i as usize).and_then(|d| *d).unwrap_or(ty);
                columns.push(ColumnType {
                    name: names.get(i as usize).cloned().unwrap_or_default(),
                    ty,
                    nullable,
                });
            }
        }

        let params = param_names
            .into_iter()
            .map(|name| Param { name, ty: None })
            .collect();

        Ok(Analysis {
            columns,
            params,
            readonly,
        })
    }

    /// Params-only analysis for a `## @sql json` query: the query assembles its
    /// own document in SQL (`json_object`/`json_group_array`), so the generated
    /// reader takes column 0 verbatim and never needs per-column types. We still
    /// prepare the statement — that validates the SQL against the schema and
    /// surfaces the named parameters — but skip the VDBE column-type walk (whose
    /// job the JSON1 functions would only confuse).
    pub fn analyze_json(&self, sql: &str) -> Result<Analysis, String> {
        let stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("prepare failed: {e}"))?;
        if !stmt.readonly() {
            return Err("@sql json query must be read-only (a SELECT)".to_string());
        }
        if stmt.column_count() < 1 {
            return Err("@sql json query must SELECT a JSON document column".to_string());
        }
        let params = self
            .param_names(&stmt)
            .into_iter()
            .map(|name| Param { name, ty: None })
            .collect();
        Ok(Analysis {
            columns: Vec::new(),
            params,
            readonly: true,
        })
    }

    fn param_names(&self, stmt: &rusqlite::Statement) -> Vec<String> {
        let n = stmt.parameter_count();
        (1..=n)
            .map(|i| match stmt.parameter_name(i) {
                // parameter_name returns the full ":viewer" token.
                Some(tok) => tok.trim_start_matches([':', '@', '$']).to_string(),
                None => i.to_string(),
            })
            .collect()
    }

    fn explain(&self, sql: &str) -> Result<Vec<Insn>, String> {
        let mut stmt = self
            .conn
            .prepare(&format!("EXPLAIN {sql}"))
            .map_err(|e| format!("prepare EXPLAIN failed: {e}"))?;
        let nparams = stmt.parameter_count();
        let nulls: Vec<rusqlite::types::Value> =
            (0..nparams).map(|_| rusqlite::types::Value::Null).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(nulls.iter()), |row| {
                Ok(Insn {
                    addr: row.get(0)?,
                    opcode: row.get(1)?,
                    p1: row.get(2)?,
                    p2: row.get(3)?,
                    p3: row.get(4)?,
                    p4: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                })
            })
            .map_err(|e| format!("run EXPLAIN failed: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read EXPLAIN failed: {e}"))
    }

    /// Map each cursor to the object it scans, plus which cursors an outer join
    /// can NULL out.
    fn resolve_cursors(&self, program: &[Insn]) -> Result<CursorMap, String> {
        let mut cursors = HashMap::new();
        let mut nullable_by_join = std::collections::HashSet::new();
        for insn in program {
            match insn.opcode.as_str() {
                "OpenRead" | "OpenWrite" => {
                    let is_index = insn.p4.starts_with("k(");
                    if let Some(c) = self.resolve_rootpage(insn.p2, is_index)? {
                        cursors.insert(insn.p1, c);
                    }
                }
                // Materialized intermediates: not schema-typable.
                "OpenEphemeral" | "OpenAutoindex" | "SorterOpen" => {
                    cursors.insert(insn.p1, Cursor::Ephemeral);
                }
                // The left-join marker: cursor p1 yields NULLs on no match.
                "NullRow" => {
                    nullable_by_join.insert(insn.p1);
                }
                _ => {}
            }
        }
        Ok(CursorMap {
            cursors,
            nullable_by_join,
        })
    }

    fn resolve_rootpage(&self, rootpage: i64, is_index: bool) -> Result<Option<Cursor>, String> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT type, name, tbl_name FROM sqlite_master WHERE rootpage = ?1",
                [rootpage],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        let Some((kind, name, tbl_name)) = row else {
            return Ok(None);
        };
        if kind == "index" || is_index {
            let cids = self.index_columns(&name)?;
            Ok(Some(Cursor::Index(tbl_name, cids)))
        } else {
            Ok(Some(Cursor::Table(name)))
        }
    }

    /// The table cids covered by an index, in index-column order (so an index
    /// `Column cursor N` maps to table column `index_columns[N]`).
    fn index_columns(&self, index_name: &str) -> Result<Vec<i64>, String> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA index_info({index_name})"))
            .map_err(|e| format!("index_info({index_name}) failed: {e}"))?;
        let cids = stmt
            .query_map([], |r| r.get::<_, i64>(1)) // (seqno, cid, name)
            .map_err(|e| format!("index_info query failed: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("index_info read failed: {e}"))?;
        Ok(cids)
    }

    /// (storage type, nullable) for table column `cid` of `table`.
    fn table_column(&self, table: &str, cid: i64) -> Result<(SqlType, bool), String> {
        // rowid alias (cid == -1) is INTEGER NOT NULL.
        if cid < 0 {
            return Ok((SqlType::Integer, false));
        }
        let row: Option<(String, i64, i64)> = self
            .conn
            .query_row(
                &format!("SELECT type, \"notnull\", pk FROM pragma_table_info('{table}') WHERE cid = ?1"),
                [cid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        let Some((decl, notnull, pk)) = row else {
            return Err(format!("no column cid={cid} in {table}"));
        };
        let ty = SqlType::from_decl(&decl);
        // INTEGER PRIMARY KEY is the rowid: never NULL even without NOT NULL.
        let nullable = notnull == 0 && !(pk == 1 && ty == SqlType::Integer);
        Ok((ty, nullable))
    }

    /// Trace register `reg`, as of instruction address `before`, back to the
    /// opcode that produced its value, and classify that into (type, nullable).
    fn type_register(
        &self,
        program: &[Insn],
        cursors: &CursorMap,
        reg: i64,
        before: i64,
    ) -> Result<(SqlType, bool), String> {
        // Guard against pathological cycles in copy chains.
        let mut budget = 256;
        let mut reg = reg;
        let mut before = before;
        loop {
            budget -= 1;
            if budget == 0 {
                return Err(format!("register {reg} provenance did not converge"));
            }
            let Some(def) = last_writer(program, reg, before) else {
                // No definite writer found: assume nullable unknown → Text/Blob
                // treated as nullable so the caller can demand an annotation.
                return Ok((SqlType::Blob, true));
            };
            match def.opcode.as_str() {
                // Transparent: value provenance is the prior writer.
                "Affinity" => {
                    before = def.addr;
                }
                "Copy" | "SCopy" | "Move" | "IntCopy" => {
                    reg = def.p1;
                    before = def.addr;
                }
                "Column" => {
                    return self.type_column(cursors, def.p1, def.p2);
                }
                "Rowid" | "IdxRowid" | "NewRowid" | "Sequence" => {
                    return Ok((SqlType::Integer, false));
                }
                "Integer" | "Int64" | "MustBeInt" => {
                    return Ok((SqlType::Integer, false));
                }
                "Real" => return Ok((SqlType::Real, false)),
                "String8" | "String" | "Blob" if def.opcode != "Blob" => {
                    return Ok((SqlType::Text, false));
                }
                "Blob" => return Ok((SqlType::Blob, false)),
                "Null" | "SoftNull" => return Ok((SqlType::Blob, true)),
                "AggFinal" | "AggValue" => {
                    return Ok(classify_aggregate(&def.p4));
                }
                "Function" | "PureFunc" | "Function0" | "PureFunc0" => {
                    return Ok(classify_function(&def.p4));
                }
                "Cast" => return Ok((SqlType::from_decl(&def.p4), true)),
                // Coroutine yields carry values in registers written elsewhere;
                // fall through to nullable-unknown rather than guess wrong.
                other => {
                    return Err(format!(
                        "unmodeled value opcode {other:?} defining register {reg} (addr {})",
                        def.addr
                    ));
                }
            }
        }
    }

    fn type_column(
        &self,
        cursors: &CursorMap,
        cursor_id: i64,
        colidx: i64,
    ) -> Result<(SqlType, bool), String> {
        // An outer join can NULL the whole row of this cursor.
        let join_nullable = cursors.nullable_by_join.contains(&cursor_id);
        match cursors.cursors.get(&cursor_id) {
            Some(Cursor::Table(table)) => {
                let (ty, nullable) = self.table_column(table, colidx)?;
                Ok((ty, nullable || join_nullable))
            }
            Some(Cursor::Index(table, cids)) => {
                let cid = cids
                    .get(colidx as usize)
                    .copied()
                    // Beyond the indexed columns SQLite appends the rowid.
                    .unwrap_or(-1);
                let (ty, nullable) = self.table_column(table, cid)?;
                Ok((ty, nullable || join_nullable))
            }
            // Materialized intermediate: unknowable type, treat as nullable.
            Some(Cursor::Ephemeral) => Ok((SqlType::Blob, true)),
            None => Err(format!("Column reads unknown cursor {cursor_id}")),
        }
    }
}

/// The highest-address instruction before `before` that writes register `reg`.
/// Covers the opcodes whose destination is p2 (the common case) and the few
/// whose destination is a different operand.
fn last_writer(program: &[Insn], reg: i64, before: i64) -> Option<&Insn> {
    program
        .iter()
        .filter(|i| i.addr < before && writes_register(i, reg))
        .max_by_key(|i| i.addr)
}

fn writes_register(insn: &Insn, reg: i64) -> bool {
    match insn.opcode.as_str() {
        // dest = p2
        "Copy" | "SCopy" | "Move" | "IntCopy" | "Integer" | "Int64" | "Real" | "String8"
        | "String" | "Variable" | "Rowid" | "IdxRowid" | "NewRowid" | "Sequence" | "MustBeInt"
        | "Cast" => insn.p2 == reg,
        // Null writes the inclusive range p2..=p3 (p3==0 means just p2).
        "Null" | "SoftNull" => {
            let hi = if insn.p3 >= insn.p2 { insn.p3 } else { insn.p2 };
            reg >= insn.p2 && reg <= hi
        }
        // dest = p3
        "Column" | "Function" | "PureFunc" | "Function0" | "PureFunc0" | "Blob" => insn.p3 == reg,
        // dest = p1 (aggregate context register)
        "AggFinal" | "AggValue" => insn.p1 == reg,
        // Affinity mutates p1 in place (affinity change, not a fresh value).
        "Affinity" => insn.p1 == reg,
        _ => false,
    }
}

/// Storage type + nullability of an aggregate given its `AggFinal` p4 (e.g.
/// "count(0)", "group_concat(1)").
fn classify_aggregate(p4: &str) -> (SqlType, bool) {
    let name = p4.split('(').next().unwrap_or("").to_ascii_lowercase();
    match name.as_str() {
        // COUNT is always a non-null integer.
        "count" => (SqlType::Integer, false),
        // TOTAL always returns a float, 0.0 for an empty group.
        "total" => (SqlType::Real, false),
        // SUM is nullable (NULL over an empty group); type follows the input,
        // default Integer for our integer schema.
        "sum" => (SqlType::Integer, true),
        "avg" => (SqlType::Real, true),
        "group_concat" | "string_agg" => (SqlType::Text, true),
        // max/min preserve the argument type; we can't see it here, so Blob
        // (any) + nullable and let an annotation pin it if needed.
        "max" | "min" => (SqlType::Blob, true),
        _ => (SqlType::Blob, true),
    }
}

fn classify_function(p4: &str) -> (SqlType, bool) {
    let name = p4.split('(').next().unwrap_or("").to_ascii_lowercase();
    match name.as_str() {
        "length" | "instr" | "abs" | "unicode" | "changes" | "total_changes"
        | "last_insert_rowid" => (SqlType::Integer, false),
        "lower" | "upper" | "trim" | "ltrim" | "rtrim" | "replace" | "substr" | "hex"
        | "quote" | "char" | "printf" | "format" => (SqlType::Text, false),
        // coalesce/ifnull/nullif and unknowns: unknown type, conservatively
        // nullable (coalesce is non-null only if a non-null arg is guaranteed).
        _ => (SqlType::Blob, true),
    }
}
