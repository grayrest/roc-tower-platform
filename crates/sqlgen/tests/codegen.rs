//! Scanner + emitter tests. The generated code's validity as Roc is covered
//! end-to-end by `just check` over the committed realworld `*Gen.roc` (Phase 5);
//! these assert the scan/emit logic and error paths.

use sqlgen::emit::emit_module;
use sqlgen::scan::{scan, Scanned};

const SOURCE: &str = r#"
## @schema
schema_users =
    \\CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL, bio TEXT)

## @schema
schema_follows =
    \\CREATE TABLE follows (follower_id INTEGER NOT NULL, followed_id INTEGER NOT NULL, PRIMARY KEY (follower_id, followed_id))

## @fragment profile_select
profile_select =
    \\SELECT u.id, u.username, u.bio,
    \\       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = u.id) AS "following: Bool"
    \\FROM users u

## @params viewer: I64, username: Str
## @sql one
load_profile =
    \\${profile_select} WHERE u.username = :username

## @params viewer: I64
## @sql
all_profiles =
    \\${profile_select}

## @params id: I64, username: Str
## @sql
insert_user =
    \\INSERT INTO users (id, username) VALUES (:id, :username)

## @params username: Str, bio: Str?, following: Bool
## @reader profile_row

## @row profile_shape
profile_shape_select =
    \\SELECT u.id, u.username, u.bio FROM users u
"#;

fn scan_ok() -> Scanned {
    scan(SOURCE).expect("scan")
}

#[test]
fn scans_schema_in_declaration_order() {
    let s = scan_ok();
    assert_eq!(s.schema.len(), 2);
    assert!(s.schema[0].contains("CREATE TABLE users"));
    assert!(s.schema[1].contains("CREATE TABLE follows"));
}

#[test]
fn keeps_trigger_begin_end_body_as_one_statement() {
    // The `;` inside a trigger body must NOT split the CREATE TRIGGER into
    // pieces; the whole `BEGIN ... END` stays one schema statement.
    let src = "## @schema\ntrg =\n    \\\\CREATE TRIGGER t AFTER INSERT ON favorites\n    \\\\BEGIN\n    \\\\  UPDATE articles SET n = n + 1 WHERE id = NEW.article_id;\n    \\\\END\n";
    let s = scan(src).expect("scan");
    assert_eq!(s.schema.len(), 1, "trigger stayed one statement: {:?}", s.schema);
    assert!(s.schema[0].contains("CREATE TRIGGER t"));
    assert!(s.schema[0].contains("UPDATE articles"));
    assert!(s.schema[0].trim_end().ends_with("END"), "got: {}", s.schema[0]);
}

#[test]
fn splices_fragment_into_queries() {
    let s = scan_ok();
    let load = s.queries.iter().find(|q| q.name == "load_profile").unwrap();
    assert!(load.sql.contains("SELECT u.id"), "fragment spliced: {}", load.sql);
    assert!(load.sql.contains("WHERE u.username = :username"));
    assert!(load.one);
}

#[test]
fn rejects_positional_placeholder_in_sql_query() {
    let src = "## @sql\nbad =\n    \\\\SELECT id FROM users WHERE id = ?\n";
    let err = scan(src).unwrap_err();
    assert!(err.contains("positional"), "got: {err}");
}

#[test]
fn rejects_unknown_fragment_reference() {
    let src = "## @sql\nbad =\n    \\\\${nope} WHERE x = :y\n";
    let err = scan(src).unwrap_err();
    assert!(err.contains("unknown fragment"), "got: {err}");
}

#[test]
fn emits_one_reader_with_nullable_and_bool() {
    let module = emit_module("Db", &scan_ok()).expect("emit");
    // The `one` reader signature and NoRows error.
    assert!(module.contains("load_profile! : U64, Str, { username : Str, viewer : I64 } => Try("));
    // OPEN union: generated errors must merge into a caller's own error union
    // so a coeffect can propagate them with bare postfix `?` (since 06f3e35).
    assert!(module.contains("[NoRows, DbErr(Str)]"));
    // bio is nullable TEXT -> NullableStr via the probe helper.
    assert!(module.contains("bio : NullableStr"));
    assert!(module.contains("col_nullable_str!"));
    // the AS "following: Bool" alias -> JsonBool.
    assert!(module.contains("following : JsonBool"));
    assert!(module.contains("JsonBool.from(Host.sql_col_i64!(handle, 3) != 0)"));
    // id is INTEGER PRIMARY KEY -> I64 NOT NULL.
    assert!(module.contains("id : I64"));
    // finishes on every path.
    assert!(module.contains("Host.sql_finish!(handle)"));
    // imports pulled in on demand.
    assert!(module.contains("import pf.NullableStr"));
    assert!(module.contains("import pf.JsonBool"));
}

#[test]
fn rejects_removed_sql_json_marker() {
    let src = "## @sql json\nbad =\n    \\\\SELECT json_object('a', 1)\n";
    let err = scan(src).unwrap_err();
    assert!(err.contains("@sql json was removed"), "got: {err}");
}

#[test]
fn emits_many_reader_returning_a_list() {
    let module = emit_module("Db", &scan_ok()).expect("emit");
    assert!(module.contains("all_profiles! : U64, Str, { viewer : I64 } => Try(List("));
    // many has no NoRows.
    assert!(module.contains("read_all_profiles! : U64, List("));
}

#[test]
fn emits_write_as_pure_builder() {
    let module = emit_module("Db", &scan_ok()).expect("emit");
    assert!(
        module.contains("insert_user : { db : Str, id : I64, username : Str } -> { db : Str, sql : Str, params : List(Host.SqlValue) }"),
        "write builder signature missing:\n{module}"
    );
    assert!(module.contains("params: [Integer(params.id), Text(params.username)]"));
}

#[test]
fn missing_param_annotation_errors() {
    let src = "## @sql\nq =\n    \\\\SELECT id FROM users WHERE id = :who\n";
    // schema so analysis can prepare.
    let s = Scanned {
        schema: vec!["CREATE TABLE users (id INTEGER PRIMARY KEY)".to_string()],
        ..scan(src).unwrap()
    };
    let err = emit_module("Db", &s).unwrap_err();
    assert!(err.contains("needs a `## @params"), "got: {err}");
}

#[test]
fn returning_write_is_rejected() {
    let one = scan("## @params\n## @sql\nbump =\n    \\\\UPDATE counters SET v = v + 1 RETURNING v\n").unwrap();
    let s = Scanned {
        schema: vec!["CREATE TABLE counters (name TEXT PRIMARY KEY, v INTEGER NOT NULL)".to_string()],
        queries: one.queries,
        ..Default::default()
    };
    let err = emit_module("Db", &s).unwrap_err();
    assert!(err.contains("RETURNING"), "got: {err}");
}

#[test]
fn emits_a_declared_reader_from_params() {
    let module = emit_module("Db", &scan_ok()).expect("emit");
    // A positional reader method in the nominal, from the declared column set.
    assert!(
        module.contains("read_profile_row! : U64, List({ username : Str, bio : NullableStr, following : JsonBool })"),
        "reader method with the declared record type"
    );
    // Positional reads, in declaration order.
    assert!(module.contains("username: Host.sql_col_str!(handle, 0)"));
    assert!(module.contains("bio: col_nullable_str!(handle, 1)"));
    assert!(module.contains("following: JsonBool.from(Host.sql_col_i64!(handle, 2) != 0)"));
}

#[test]
fn reader_without_params_is_an_error() {
    let src = "## @reader lonely
";
    let err = scan(src).unwrap_err();
    assert!(err.contains("needs a preceding @params"), "got: {err}");
}

#[test]
fn emits_a_row_decoder_from_the_analyzed_shape() {
    let module = emit_module("Db", &scan_ok()).expect("emit");
    // A `@row` analyzes the SELECT's columns (no begin/read/finish wrapper) and
    // emits a typed record + a List(SqlValue) -> record borrowed-cell decoder.
    assert!(
        module.contains(
            "decode_profile_shape : List(Host.SqlValue) -> { id : I64, username : Str, bio : NullableStr }"
        ),
        "row decoder signature missing:\n{module}"
    );
    // Cells read positionally from the borrowed row: I64 non-null, Str non-null,
    // nullable TEXT via the borrowed nullable helper.
    assert!(module.contains(
        "decode_profile_shape = |cols| { id: cell_i64(cols, 0), username: cell_str(cols, 1), bio: cell_nullable_str(cols, 2) }"
    ));
    // The pure cell helpers are emitted for exactly the kinds used.
    assert!(module.contains("cell_i64 : List(Host.SqlValue), U64 -> I64"));
    assert!(module.contains("cell_str : List(Host.SqlValue), U64 -> Str"));
    assert!(module.contains("cell_nullable_str : List(Host.SqlValue), U64 -> NullableStr"));
    // ...and NOT the unused ones.
    assert!(!module.contains("cell_f64 :"), "unused cell_f64 helper emitted");
    assert!(!module.contains("cell_blob :"), "unused cell_blob helper emitted");
}

#[test]
fn row_rejects_params_annotation() {
    let src = "## @params x: I64\n## @row bad\nbad_select =\n    \\\\SELECT 1\n";
    let err = scan(src).unwrap_err();
    assert!(err.contains("@params is not valid on @row"), "got: {err}");
}

#[test]
fn row_without_name_is_an_error() {
    let err = scan("## @row\nx =\n    \\\\SELECT 1\n").unwrap_err();
    assert!(err.contains("@row requires a name"), "got: {err}");
}
