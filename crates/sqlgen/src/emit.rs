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

    /// The expression reading column `idx` from a borrowed `List(SqlValue)` row
    /// (`cols`) — the `@row` decoder path, PURE, zero-copy for Text/Blob cells.
    fn decode_expr(&self, idx: usize) -> String {
        match self {
            RocType::I64 => format!("cell_i64(cols, {idx})"),
            RocType::F64 => format!("cell_f64(cols, {idx})"),
            RocType::Str => format!("cell_str(cols, {idx})"),
            RocType::Blob => format!("cell_blob(cols, {idx})"),
            RocType::JsonBool => format!("JsonBool.from(cell_i64(cols, {idx}) != 0)"),
            RocType::NullableStr => format!("cell_nullable_str(cols, {idx})"),
            RocType::NullableI64 => format!("cell_nullable_i64(cols, {idx})"),
        }
    }

    /// The name of the top-level `cell_*` helper `decode_expr` references, so
    /// emit_module can emit exactly the helpers a row shape uses.
    fn cell_helper(&self) -> &'static str {
        match self {
            RocType::I64 | RocType::JsonBool => "cell_i64",
            RocType::F64 => "cell_f64",
            RocType::Str => "cell_str",
            RocType::Blob => "cell_blob",
            RocType::NullableStr => "cell_nullable_str",
            RocType::NullableI64 => "cell_nullable_i64",
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

    for query in &scanned.queries {
        let analysis = analyzer
            .analyze(&query.sql)
            .map_err(|e| format!("analyze {:?}: {e}", query.name))?;
        let emitted = emit_query(query, &analysis)?;
        needs_nullable_str |= emitted.uses_nullable_str;
        needs_nullable_i64 |= emitted.uses_nullable_i64;
        needs_jsonbool |= emitted.uses_jsonbool;
        fns.push(emitted.decl);
        helpers.push(emitted.helper);
    }

    for reader in &scanned.readers {
        let emitted = emit_reader(reader)?;
        needs_nullable_str |= emitted.uses_nullable_str;
        needs_nullable_i64 |= emitted.uses_nullable_i64;
        needs_jsonbool |= emitted.uses_jsonbool;
        fns.push(emitted.decl);
    }

    // `@row` decoders: analyze the SELECT's column shape and emit a typed row
    // record + a `List(SqlValue) -> record` borrowed-cell decoder (no query
    // wrapper). The `cell_*` helpers it references are emitted once below.
    let mut needs_cell: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    for row in &scanned.rows {
        let analysis = analyzer
            .analyze(&row.sql)
            .map_err(|e| format!("analyze @row {:?}: {e}", row.name))?;
        let fields = resolve_fields(&analysis)?;
        needs_nullable_str |= fields.iter().any(|f| f.ty == RocType::NullableStr);
        needs_nullable_i64 |= fields.iter().any(|f| f.ty == RocType::NullableI64);
        needs_jsonbool |= fields.iter().any(|f| f.ty == RocType::JsonBool);
        for f in &fields {
            needs_cell.insert(f.ty.cell_helper());
        }
        fns.push(emit_row_decoder(&row.name, &fields));
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
    // Shared borrowed-cell decode helpers for `@row` decoders (pure, over a
    // `List(SqlValue)` row), emitted once for exactly the kinds used.
    for name in &needs_cell {
        out.push_str(cell_helper_src(name));
        out.push('\n');
    }
    Ok(out)
}

/// A `@row` decoder: a typed record + a `List(SqlValue) -> record` reader,
/// emitted as a `DbGen` method (`DbGen.decode_<name>`). Text/Blob cells are
/// passed through borrowed (zero copy); the caller supplies each row from the
/// borrowed-slice cursor fold.
fn emit_row_decoder(name: &str, fields: &[Field]) -> String {
    let record_ty = record_type(fields);
    let assigns = fields
        .iter()
        .enumerate()
        .map(|(idx, f)| format!("{}: {}", f.name, f.ty.decode_expr(idx)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "    decode_{name} : List(Host.SqlValue) -> {record_ty}\n\
         \x20   decode_{name} = |cols| {{ {assigns} }}",
        name = name,
        record_ty = record_ty,
        assigns = assigns,
    )
}

/// Source for one `cell_*` decode helper by name.
fn cell_helper_src(name: &str) -> &'static str {
    match name {
        "cell_i64" => CELL_I64_HELPER,
        "cell_f64" => CELL_F64_HELPER,
        "cell_str" => CELL_STR_HELPER,
        "cell_blob" => CELL_BLOB_HELPER,
        "cell_nullable_str" => CELL_NULLABLE_STR_HELPER,
        "cell_nullable_i64" => CELL_NULLABLE_I64_HELPER,
        other => unreachable!("unknown cell helper {other}"),
    }
}

struct EmittedQuery {
    decl: String,
    helper: String,
    uses_nullable_str: bool,
    uses_nullable_i64: bool,
    uses_jsonbool: bool,
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

    if analysis.readonly {
        emit_read(query, analysis, &sql_literal, &bindings, &param_record_ty)
    } else {
        emit_write(query, &sql_literal, &bindings, &params)
    }
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
            format!("Try({record_ty}, [NoRows, DbErr(Str), ..])"),
            format!(
                "{helper}! : U64 => Try({record_ty}, [NoRows, DbErr(Str), ..])\n\
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
            format!("Try(List({record_ty}), [DbErr(Str), ..])"),
            format!(
                "{helper}! : U64, List({record_ty}) => Try(List({record_ty}), [DbErr(Str), ..])\n\
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
    })
}

/// Heuristic: a write "returns no rows" unless its SQL has a RETURNING clause.
fn query_returns_no_rows(query: &Query) -> bool {
    !query.sql.to_ascii_uppercase().contains("RETURNING")
}

/// Build a positional row reader as a `DbGen` method from a DECLARED column
/// set (`## @reader`), for a dynamic query whose SQL has no analyzable literal.
/// Fields read in declaration order, which the caller's SELECT must match.
fn emit_reader(reader: &crate::scan::Reader) -> Result<EmittedQuery, String> {
    let fields: Vec<Field> = reader
        .fields
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            let ty = RocType::from_annotation(&p.ty)
                .ok_or_else(|| format!("@reader {:?} column {:?}: unknown type {:?}", reader.name, p.name, p.ty))?;
            let read = ty.read_expr(idx);
            Ok(Field { name: p.name.clone(), ty, read })
        })
        .collect::<Result<_, String>>()?;
    let record_ty = record_type(&fields);
    let record_lit = record_literal(&fields);
    let name = &reader.name;
    // A METHOD in the nominal block (callable as `DbGen.read_NAME!`), so a
    // hand-written dynamic query can open its cursor and step rows through it.
    let decl = format!(
        "    read_{name}! : U64, List({record_ty}) => Try(List({record_ty}), [DbErr(Str), ..])
             read_{name}! = |handle, acc| {{
                 match Host.sql_step!(handle) {{
                     Err(message) => Err(DbErr(message))
                     Ok(has_row) =>
                         if has_row {{
                             read_{name}!(handle, acc.append({record_lit}))
                         }} else {{
                             Ok(acc)
                         }}
                 }}
             }}",
        name = name,
        record_ty = record_ty,
        record_lit = record_lit,
    );
    Ok(EmittedQuery {
        decl,
        helper: String::new(),
        uses_nullable_str: fields.iter().any(|f| f.ty == RocType::NullableStr),
        uses_nullable_i64: fields.iter().any(|f| f.ty == RocType::NullableI64),
        uses_jsonbool: fields.iter().any(|f| f.ty == RocType::JsonBool),
    })
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

// Borrowed-cell decode helpers (pure) for `@row` decoders. Each reads column
// `index` of a `List(SqlValue)` row; Text/Blob return the borrowed cell without
// copying. A missing/type-mismatched cell falls back to the kind's zero value.
const CELL_I64_HELPER: &str = "\
## Read column `index` of a borrowed row as I64 (0 if absent/non-integer).
cell_i64 : List(Host.SqlValue), U64 -> I64
cell_i64 = |cols, index| match List.get(cols, index) {
    Ok(Integer(n)) => n
    _ => 0
}";

const CELL_F64_HELPER: &str = "\
## Read column `index` of a borrowed row as F64 (0.0 if absent/non-real).
cell_f64 : List(Host.SqlValue), U64 -> F64
cell_f64 = |cols, index| match List.get(cols, index) {
    Ok(Real(f)) => f
    _ => 0.0
}";

const CELL_STR_HELPER: &str = "\
## Read column `index` of a borrowed row as Str (empty if absent/non-text).
cell_str : List(Host.SqlValue), U64 -> Str
cell_str = |cols, index| match List.get(cols, index) {
    Ok(Text(s)) => s
    _ => \"\"
}";

const CELL_BLOB_HELPER: &str = "\
## Read column `index` of a borrowed row as bytes (empty if absent/non-blob).
cell_blob : List(Host.SqlValue), U64 -> List(U8)
cell_blob = |cols, index| match List.get(cols, index) {
    Ok(Blob(b)) => b
    _ => []
}";

const CELL_NULLABLE_STR_HELPER: &str = "\
## Read column `index` of a borrowed row as NullableStr (Null if absent/NULL).
cell_nullable_str : List(Host.SqlValue), U64 -> NullableStr
cell_nullable_str = |cols, index| match List.get(cols, index) {
    Ok(Text(s)) => NullableStr.Val(s)
    _ => NullableStr.Null
}";

const CELL_NULLABLE_I64_HELPER: &str = "\
## Read column `index` of a borrowed row as NullableI64 (Null if absent/NULL).
cell_nullable_i64 : List(Host.SqlValue), U64 -> NullableI64
cell_nullable_i64 = |cols, index| match List.get(cols, index) {
    Ok(Integer(n)) => NullableI64.Val(n)
    _ => NullableI64.Null
}";
