//! Emit a generated Roc module from scanned + analyzed queries.
//!
//! Reads become effectful typed functions that stream the cursor and read
//! cells positionally into a flat record (no `Sql.Row` materialization).
//! Writes become pure builders returning a `DbWrite` effect payload. Column
//! types come from the analyzer (overridable by an `AS "name: Type"` alias
//! suffix); param types come from `## @params` (v1 requires them — the
//! analyzer types columns, not parameters).

use crate::analyze::{Analysis, Analyzer, SqlType};
use crate::scan::{Query, Scanned};
use rusqlite::Connection;

/// Resolve a column/param annotation or analyzed type to a Roc type and its
/// per-cell read / bind fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RocType {
    I64,
    F64,
    Str,
    Blob,
    NullableStr,
    NullableI64,
    JsonBool,
}

impl RocType {
    fn annotation(&self) -> &'static str {
        match self {
            RocType::I64 => "I64",
            RocType::F64 => "F64",
            RocType::Str => "Str",
            RocType::Blob => "List(U8)",
            RocType::NullableStr => "NullableStr",
            RocType::NullableI64 => "NullableI64",
            RocType::JsonBool => "JsonBool",
        }
    }

    /// The expression reading column `idx` of the current row.
    fn read_expr(&self, idx: usize) -> String {
        match self {
            RocType::I64 => format!("Host.sql_col_i64!(handle, {idx})"),
            RocType::F64 => format!("Host.sql_col_f64!(handle, {idx})"),
            RocType::Str => format!("Host.sql_col_str!(handle, {idx})"),
            RocType::Blob => format!("Host.sql_col_blob!(handle, {idx})"),
            RocType::JsonBool => format!("JsonBool.from(Host.sql_col_i64!(handle, {idx}) != 0)"),
            RocType::NullableStr => format!("col_nullable_str!(handle, {idx})"),
            RocType::NullableI64 => format!("col_nullable_i64!(handle, {idx})"),
        }
    }

    /// The expression binding a param field to a Host.SqlValue.
    fn bind_expr(&self, field: &str) -> Result<String, String> {
        match self {
            RocType::I64 => Ok(format!("Integer(params.{field})")),
            RocType::F64 => Ok(format!("Real(params.{field})")),
            RocType::Str => Ok(format!("Text(params.{field})")),
            RocType::Blob => Ok(format!("Blob(params.{field})")),
            other => Err(format!(
                "param {field:?} has type {} — only I64/F64/Str/List(U8) params are supported",
                other.annotation()
            )),
        }
    }

    fn from_annotation(annot: &str) -> Option<RocType> {
        match annot.trim() {
            "I64" => Some(RocType::I64),
            "F64" => Some(RocType::F64),
            "Str" => Some(RocType::Str),
            "List(U8)" | "Blob" => Some(RocType::Blob),
            "Str?" => Some(RocType::NullableStr),
            "I64?" => Some(RocType::NullableI64),
            "Bool" => Some(RocType::JsonBool),
            _ => None,
        }
    }

    fn from_analysis(ty: SqlType, nullable: bool) -> Result<RocType, String> {
        Ok(match (ty, nullable) {
            (SqlType::Integer, false) => RocType::I64,
            (SqlType::Integer, true) => RocType::NullableI64,
            (SqlType::Real, false) => RocType::F64,
            (SqlType::Text, false) => RocType::Str,
            (SqlType::Text, true) => RocType::NullableStr,
            (SqlType::Blob, _) => RocType::Blob,
            (SqlType::Real, true) => {
                return Err("nullable REAL column needs an explicit alias annotation".to_string())
            }
        })
    }
}

struct Field {
    name: String,
    ty: RocType,
    read: String,
}

/// Emit `module_name`Gen.roc from all scanned queries, analyzed against the
/// scanned schema.
pub fn emit_module(module_name: &str, scanned: &Scanned) -> Result<String, String> {
    let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
    for ddl in &scanned.schema {
        conn.execute(ddl, [])
            .map_err(|e| format!("schema apply failed for {ddl:?}: {e}"))?;
    }
    let analyzer = Analyzer::new(&conn);

    let gen_name = format!("{module_name}Gen");
    let mut fns: Vec<String> = Vec::new();
    let mut helpers: Vec<String> = Vec::new();
    let mut needs_nullable_str = false;
    let mut needs_nullable_i64 = false;
    let mut needs_jsonbool = false;
    let mut needs_rawjson = false;

    for query in &scanned.queries {
        let analysis = if query.json {
            analyzer.analyze_json(&query.sql)
        } else {
            analyzer.analyze(&query.sql)
        }
        .map_err(|e| format!("analyze {:?}: {e}", query.name))?;
        let emitted = emit_query(query, &analysis)?;
        needs_nullable_str |= emitted.uses_nullable_str;
        needs_nullable_i64 |= emitted.uses_nullable_i64;
        needs_jsonbool |= emitted.uses_jsonbool;
        needs_rawjson |= emitted.uses_rawjson;
        fns.push(emitted.decl);
        helpers.push(emitted.helper);
    }

    let mut out = String::new();
    out.push_str("## Generated by sqlgen — DO NOT EDIT. Regenerate with `just sqlgen`.\n");
    // Imports precede the nominal block: qualified `Host.xxx` references inside
    // the block's associated methods only bind to an import that appears BEFORE
    // the block. Generated modules live in app code, so platform modules carry
    // the `pf.` package prefix.
    out.push_str("import pf.Host\n");
    if needs_nullable_str {
        out.push_str("import pf.NullableStr\n");
    }
    if needs_nullable_i64 {
        out.push_str("import pf.NullableI64\n");
    }
    if needs_jsonbool {
        out.push_str("import pf.JsonBool\n");
    }
    if needs_rawjson {
        out.push_str("import pf.RawJson\n");
    }
    out.push('\n');
    out.push_str(&format!("{gen_name} := [].{{\n"));
    for f in &fns {
        out.push_str(f);
        out.push('\n');
    }
    out.push_str("}\n\n");
    // Shared nullable-cell helpers, emitted once if any query needs them.
    if needs_nullable_str {
        out.push_str(NULLABLE_STR_HELPER);
        out.push('\n');
    }
    if needs_nullable_i64 {
        out.push_str(NULLABLE_I64_HELPER);
        out.push('\n');
    }
    for h in &helpers {
        if !h.is_empty() {
            out.push_str(h);
            out.push('\n');
        }
    }
    Ok(out)
}

struct EmittedQuery {
    decl: String,
    helper: String,
    uses_nullable_str: bool,
    uses_nullable_i64: bool,
    uses_jsonbool: bool,
    uses_rawjson: bool,
}

fn emit_query(query: &Query, analysis: &Analysis) -> Result<EmittedQuery, String> {
    // Param types from @params (required in v1).
    let params = resolve_params(query, analysis)?;
    let sql_literal = roc_string_literal(&collapse_ws(&query.sql));
    let bindings = params
        .iter()
        .map(|(name, ty)| ty.bind_expr(name))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let param_record_ty = param_record_type(&params);

    if query.json {
        emit_read_json(query, &sql_literal, &bindings, &param_record_ty)
    } else if analysis.readonly {
        emit_read(query, analysis, &sql_literal, &bindings, &param_record_ty)
    } else {
        emit_write(query, &sql_literal, &bindings, &params)
    }
}

/// Emit a `## @sql json` reader: the query built its whole JSON document in SQL
/// (`json_object`/`json_group_array`), so the reader reads column 0 verbatim
/// into a `RawJson`, skipping all per-column decode. `json one` returns a single
/// document with a `NoRows` miss (404-shaped); plain `json` reads the single
/// aggregate row (a query that never returns zero rows by construction).
fn emit_read_json(
    query: &Query,
    sql_literal: &str,
    bindings: &str,
    param_record_ty: &str,
) -> Result<EmittedQuery, String> {
    let name = &query.name;
    let helper_name = format!("read_{name}");
    let read_doc = "Ok(RawJson.from(Host.sql_col_str!(handle, 0)))";

    let (ret_ty, helper) = if query.one {
        (
            "Try(RawJson, [NoRows, DbErr(Str)])".to_string(),
            format!(
                "{helper}! : U64 => Try(RawJson, [NoRows, DbErr(Str)])\n\
                 {helper}! = |handle| {{\n\
                 \x20   match Host.sql_step!(handle) {{\n\
                 \x20       Err(message) => Err(DbErr(message))\n\
                 \x20       Ok(has_row) =>\n\
                 \x20           if has_row {{\n\
                 \x20               {read_doc}\n\
                 \x20           }} else {{\n\
                 \x20               Err(NoRows)\n\
                 \x20           }}\n\
                 \x20   }}\n\
                 }}",
                helper = helper_name,
            ),
        )
    } else {
        (
            "Try(RawJson, [DbErr(Str)])".to_string(),
            format!(
                "{helper}! : U64 => Try(RawJson, [DbErr(Str)])\n\
                 {helper}! = |handle| {{\n\
                 \x20   match Host.sql_step!(handle) {{\n\
                 \x20       Err(message) => Err(DbErr(message))\n\
                 \x20       Ok(has_row) =>\n\
                 \x20           if has_row {{\n\
                 \x20               {read_doc}\n\
                 \x20           }} else {{\n\
                 \x20               Err(DbErr(\"@sql json query returned no rows\"))\n\
                 \x20           }}\n\
                 \x20   }}\n\
                 }}",
                helper = helper_name,
            ),
        )
    };

    let params_arg = if bindings.is_empty() { "_params" } else { "params" };
    let decl = format!(
        "    {name}! : U64, Str, {param_ty} => {ret_ty}\n\
         \x20   {name}! = |ctx, db, {params_arg}| {{\n\
         \x20       handle = Host.sql_begin!({{ ctx: ctx, db: db, sql: {sql}, params: [{bindings}] }}) ? |message| DbErr(message)\n\
         \x20       result = {helper}!(handle)\n\
         \x20       Host.sql_finish!(handle)\n\
         \x20       result\n\
         \x20   }}",
        name = name,
        param_ty = param_record_ty,
        ret_ty = ret_ty,
        sql = sql_literal,
        helper = helper_name,
        params_arg = params_arg,
    );

    Ok(EmittedQuery {
        decl,
        helper,
        uses_nullable_str: false,
        uses_nullable_i64: false,
        uses_jsonbool: false,
        uses_rawjson: true,
    })
}

fn emit_read(
    query: &Query,
    analysis: &Analysis,
    sql_literal: &str,
    bindings: &str,
    param_record_ty: &str,
) -> Result<EmittedQuery, String> {
    let fields = resolve_fields(analysis)?;
    let record_ty = record_type(&fields);
    let record_lit = record_literal(&fields);
    let name = &query.name;
    let helper_name = format!("read_{name}");

    let (ret_ty, helper) = if query.one {
        (
            format!("Try({record_ty}, [NoRows, DbErr(Str)])"),
            format!(
                "{helper}! : U64 => Try({record_ty}, [NoRows, DbErr(Str)])\n\
                 {helper}! = |handle| {{\n\
                 \x20   match Host.sql_step!(handle) {{\n\
                 \x20       Err(message) => Err(DbErr(message))\n\
                 \x20       Ok(has_row) =>\n\
                 \x20           if has_row {{\n\
                 \x20               Ok({record_lit})\n\
                 \x20           }} else {{\n\
                 \x20               Err(NoRows)\n\
                 \x20           }}\n\
                 \x20   }}\n\
                 }}",
                helper = helper_name,
                record_ty = record_ty,
                record_lit = record_lit,
            ),
        )
    } else {
        (
            format!("Try(List({record_ty}), [DbErr(Str)])"),
            format!(
                "{helper}! : U64, List({record_ty}) => Try(List({record_ty}), [DbErr(Str)])\n\
                 {helper}! = |handle, acc| {{\n\
                 \x20   match Host.sql_step!(handle) {{\n\
                 \x20       Err(message) => Err(DbErr(message))\n\
                 \x20       Ok(has_row) =>\n\
                 \x20           if has_row {{\n\
                 \x20               {helper}!(handle, acc.append({record_lit}))\n\
                 \x20           }} else {{\n\
                 \x20               Ok(acc)\n\
                 \x20           }}\n\
                 \x20   }}\n\
                 }}",
                helper = helper_name,
                record_ty = record_ty,
                record_lit = record_lit,
            ),
        )
    };

    let call = if query.one {
        format!("{helper_name}!(handle)")
    } else {
        format!("{helper_name}!(handle, [])")
    };

    let params_arg = if bindings.is_empty() { "_params" } else { "params" };
    let decl = format!(
        "    {name}! : U64, Str, {param_ty} => {ret_ty}\n\
         \x20   {name}! = |ctx, db, {params_arg}| {{\n\
         \x20       handle = Host.sql_begin!({{ ctx: ctx, db: db, sql: {sql}, params: [{bindings}] }}) ? |message| DbErr(message)\n\
         \x20       result = {call}\n\
         \x20       Host.sql_finish!(handle)\n\
         \x20       result\n\
         \x20   }}",
        name = name,
        param_ty = param_record_ty,
        ret_ty = ret_ty,
        sql = sql_literal,
        bindings = bindings,
        call = call,
        params_arg = params_arg,
    );

    Ok(EmittedQuery {
        decl,
        helper,
        uses_nullable_str: fields.iter().any(|f| f.ty == RocType::NullableStr),
        uses_nullable_i64: fields.iter().any(|f| f.ty == RocType::NullableI64),
        uses_jsonbool: fields.iter().any(|f| f.ty == RocType::JsonBool),
        uses_rawjson: false,
    })
}

fn emit_write(
    query: &Query,
    sql_literal: &str,
    bindings: &str,
    params: &[(String, RocType)],
) -> Result<EmittedQuery, String> {
    // A write that returns rows (RETURNING) can't flow back through the
    // post-handler effect list — reject it.
    if !query_returns_no_rows(query) {
        return Err(format!(
            "@sql write {:?} uses RETURNING; RETURNING writes stay hand-written (Sql.query!)",
            query.name
        ));
    }
    // A write builder takes the DbWrite target `db` alongside the SQL params.
    let mut with_db: Vec<(String, RocType)> = params.to_vec();
    with_db.push(("db".to_string(), RocType::Str));
    let param_record_ty = param_record_type(&with_db);
    let name = &query.name;
    let decl = format!(
        "    {name} : {param_ty} -> {{ db : Str, sql : Str, params : List(Host.SqlValue) }}\n\
         \x20   {name} = |params| {{ db: params.db, sql: {sql}, params: [{bindings}] }}",
        name = name,
        param_ty = param_record_ty,
        sql = sql_literal,
        bindings = bindings,
    );
    Ok(EmittedQuery {
        decl,
        helper: String::new(),
        uses_nullable_str: false,
        uses_nullable_i64: false,
        uses_jsonbool: false,
        uses_rawjson: false,
    })
}

/// Heuristic: a write "returns no rows" unless its SQL has a RETURNING clause.
fn query_returns_no_rows(query: &Query) -> bool {
    !query.sql.to_ascii_uppercase().contains("RETURNING")
}

fn resolve_fields(analysis: &Analysis) -> Result<Vec<Field>, String> {
    analysis
        .columns
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            // An `AS "name: Type"` alias suffix overrides the analyzed type.
            let (field_name, ty) = if let Some((base, annot)) = col.name.split_once(':') {
                let ty = RocType::from_annotation(annot)
                    .ok_or_else(|| format!("column {:?}: unknown type annotation {annot:?}", col.name))?;
                (base.trim().to_string(), ty)
            } else {
                (col.name.clone(), RocType::from_analysis(col.ty, col.nullable)?)
            };
            let read = ty.read_expr(idx);
            Ok(Field { name: field_name, ty, read })
        })
        .collect()
}

fn resolve_params(query: &Query, analysis: &Analysis) -> Result<Vec<(String, RocType)>, String> {
    analysis
        .params
        .iter()
        .map(|p| {
            let annot = query
                .params
                .iter()
                .find(|a| a.name == p.name)
                .ok_or_else(|| {
                    format!(
                        "query {:?}: parameter :{} needs a `## @params {}: <Type>` annotation",
                        query.name, p.name, p.name
                    )
                })?;
            let ty = RocType::from_annotation(&annot.ty)
                .ok_or_else(|| format!("param :{}: unknown type {:?}", p.name, annot.ty))?;
            Ok((p.name.clone(), ty))
        })
        .collect()
}

fn param_record_type(params: &[(String, RocType)]) -> String {
    if params.is_empty() {
        return "{}".to_string();
    }
    let mut sorted: Vec<&(String, RocType)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let inner = sorted
        .iter()
        .map(|(n, t)| format!("{n} : {}", t.annotation()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {inner} }}")
}

fn record_type(fields: &[Field]) -> String {
    let inner = fields
        .iter()
        .map(|f| format!("{} : {}", f.name, f.ty.annotation()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {inner} }}")
}

fn record_literal(fields: &[Field]) -> String {
    let inner = fields
        .iter()
        .map(|f| format!("{}: {}", f.name, f.read))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {inner} }}")
}

/// Collapse runs of whitespace (incl. newlines) to single spaces for a compact
/// one-line SQL literal (SQL is whitespace-insensitive).
fn collapse_ws(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn roc_string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

const NULLABLE_STR_HELPER: &str = "\
## Read a nullable TEXT cell into NullableStr.
col_nullable_str! : U64, U64 => NullableStr
col_nullable_str! = |handle, index| {
    match Host.sql_col_type!(handle, index) {
        Null => NullableStr.Null
        _ => NullableStr.Val(Host.sql_col_str!(handle, index))
    }
}";

const NULLABLE_I64_HELPER: &str = "\
## Read a nullable INTEGER cell into NullableI64.
col_nullable_i64! : U64, U64 => NullableI64
col_nullable_i64! = |handle, index| {
    match Host.sql_col_type!(handle, index) {
        Null => NullableI64.Null
        _ => NullableI64.Val(Host.sql_col_i64!(handle, index))
    }
}";
