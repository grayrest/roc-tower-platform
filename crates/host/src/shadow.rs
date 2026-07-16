//! Analysis shadow: cache-dependency derivation via the SQLite authorizer.
//!
//! One in-memory rusqlite connection per database NAME mirrors that database's
//! schema (mirrored from init-time statements — see `mirror_schema`). Deriving
//! what a statement reads and writes = preparing it against the shadow with
//! `Connection::authorizer` installed and collecting the callbacks: SQLite
//! compiles trigger programs into the statement at prepare time, so a write's
//! trigger-carried updates (`accessor` = the firing trigger) are reported
//! here with NO trigger-body parsing — that IS the cascade expansion the
//! response cache's invalidation relies on. Results are memoized per SQL
//! text, so the request hot path pays a lock + map hit after warmup, on both
//! engines (turso has no authorizer; the shadow serves both identically).
//!
//! Soundness posture is MAY-analysis + fail-closed: ambiguity degrades to
//! `Cols::All` (over-purge, never staleness); anything unattributable marks
//! the analysis `opaque` (read side → uncacheable, write side → conservative
//! purge). The shadow must never `prepare_cached` — a cached-statement hit
//! skips `sqlite3_prepare` entirely and would memoize an EMPTY set as truth.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;

/// Column scope of a table touch. `All` is the sound floor (inserts/deletes,
/// wildcard or unattributable reads); `Named` enables column-scoped purging
/// (an `UPDATE articles SET favorites_count` spares entries that never read
/// that column).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cols {
    All,
    Named(HashSet<Box<str>>),
}

impl Cols {
    fn add(&mut self, column: &str) {
        if column.is_empty() {
            *self = Cols::All;
            return;
        }
        match self {
            Cols::All => {}
            Cols::Named(set) => {
                set.insert(Box::from(column));
            }
        }
    }

    fn widen(&mut self) {
        *self = Cols::All;
    }

    /// Does a write of `self` columns touch a read of `other` columns?
    /// `All` on either side always intersects.
    pub fn intersects(&self, other: &Cols) -> bool {
        match (self, other) {
            (Cols::All, _) | (_, Cols::All) => true,
            (Cols::Named(a), Cols::Named(b)) => a.iter().any(|c| b.contains(c)),
        }
    }

    /// Union `other` into self (MAY-analysis: widening only).
    pub fn union(&mut self, other: &Cols) {
        match (&mut *self, other) {
            (Cols::All, _) => {}
            (_, Cols::All) => *self = Cols::All,
            (Cols::Named(mine), Cols::Named(theirs)) => {
                mine.extend(theirs.iter().cloned());
            }
        }
    }
}

/// Keyword gate for schema MIRRORING only (analysis-side DDL detection uses
/// the authorizer). A mid-request `migrate!` runs under ctx 0; without this
/// gate its DML would replay into the shadow, growing it with data rows. A
/// missed exotic DDL shape degrades safely: the shadow lacks the schema, the
/// derivation fails, the read is uncacheable (fail closed).
pub fn looks_like_ddl(sql: &str) -> bool {
    let head = sql.trim_start();
    ["CREATE", "ALTER", "DROP"]
        .iter()
        .any(|kw| head.len() >= kw.len() && head[..kw.len()].eq_ignore_ascii_case(kw))
}

/// Everything the authorizer reported about one SQL text, interpreted.
/// Read consumers: `opaque || schema_change` → uncacheable; non-empty
/// `writes` → purge them AND poison (a "read" that mutates escaped the
/// effect choke point — R2). Write consumers: `schema_change` → the shadow
/// already applied the DDL and flushed the memo, purge everything;
/// otherwise purge `writes` per-column.
#[derive(Debug, Default)]
pub struct SqlAnalysis {
    pub reads: HashMap<Box<str>, Cols>,
    pub writes: HashMap<Box<str>, Cols>,
    /// The statement calls a nondeterministic SQL function (`random()`,
    /// date/time family) — the response may vary run-to-run; poison.
    /// Documented exclusion: bare `CURRENT_TIMESTAMP` is a keyword and fires
    /// no `Function` callback — not catchable via the authorizer.
    pub nondeterministic: bool,
    /// DDL / schema-shaped action observed (CREATE/DROP/ALTER/ATTACH…).
    pub schema_change: bool,
    /// An authorizer action we could not attribute to a table. Fail closed.
    pub opaque: bool,
}

/// One shadow per database name (R5: a single global shadow would mix
/// multi-db DDL — a colliding `CREATE TABLE IF NOT EXISTS` silently no-ops
/// and drops the second db's triggers → under-purge).
struct Shadow {
    conn: Mutex<Connection>,
    memo: Mutex<HashMap<Box<str>, Arc<SqlAnalysis>>>,
}

/// Bound on per-db memoized analyses: value-spliced dynamic SQL would grow
/// the map without bound (R15). At the cap new texts derive uncached each
/// time (warm entries stay), degrading speed, never correctness.
const MEMO_CAP: usize = 4096;

// Raw SQLite authorizer codes rusqlite maps to `Unknown` when an expected
// string argument is NULL (e.g. SQLITE_READ with a NULL column for
// `count(*)`-shaped access). We recover the table when present.
const SQLITE_READ: i32 = 20;
const SQLITE_UPDATE: i32 = 23;

/// SQL functions whose result varies run-to-run. The date/time family is
/// only nondeterministic with a `'now'` argument, but arguments are not
/// visible in the callback — over-approximate (poison), which costs hit
/// rate, never correctness.
const NONDETERMINISTIC_FNS: &[&str] = &[
    "random",
    "randomblob",
    "date",
    "time",
    "datetime",
    "julianday",
    "unixepoch",
    "strftime",
    "timediff",
];

fn shadows() -> &'static Mutex<HashMap<Box<str>, Arc<Shadow>>> {
    static SHADOWS: OnceLock<Mutex<HashMap<Box<str>, Arc<Shadow>>>> = OnceLock::new();
    SHADOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn shadow_for(db: &str) -> Result<Arc<Shadow>, String> {
    let mut map = shadows().lock().expect("shadow registry poisoned");
    if let Some(shadow) = map.get(db) {
        return Ok(shadow.clone());
    }
    let conn = Connection::open_in_memory().map_err(|err| err.to_string())?;
    // R6: FK cascade writes are only compiled (hence only reported by the
    // authorizer) when the pragma is on. Unconditionally ON is a sound
    // over-approximation even if the live engine runs with FKs off.
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|err| err.to_string())?;
    let shadow = Arc::new(Shadow {
        conn: Mutex::new(conn),
        memo: Mutex::new(HashMap::new()),
    });
    map.insert(Box::from(db), shadow.clone());
    Ok(shadow)
}

/// Mirror an init-time (`ctx == 0`) statement into `db`'s shadow so later
/// derivations see the schema. Executes to completion (DDL must take
/// effect); failures are logged, not fatal — a parameterized seed INSERT
/// can't replay here and doesn't need to (only DDL shapes derivation).
pub fn mirror_schema(db: &str, sql: &str) {
    let shadow = match shadow_for(db) {
        Ok(shadow) => shadow,
        Err(err) => {
            tracing::warn!(db, %err, "shadow: could not open for schema mirror");
            return;
        }
    };
    let conn = shadow.conn.lock().expect("shadow conn poisoned");
    if let Err(err) = conn.execute_batch(sql) {
        tracing::warn!(db, %err, sql, "shadow: schema statement did not mirror");
    }
    drop(conn);
    shadow.memo.lock().expect("shadow memo poisoned").clear();
}

/// Derive (memoized) what `sql` reads and writes against `db`'s schema.
/// `Err` = the shadow couldn't prepare the statement (schema divergence,
/// engine-specific syntax) — callers fail closed (read → uncacheable,
/// write → conservative purge). Errors are never memoized. A `schema_change`
/// analysis applies the DDL to the shadow and flushes the memo before
/// returning (R4) and is itself never memoized.
pub fn analyze(db: &str, sql: &str) -> Result<Arc<SqlAnalysis>, String> {
    let shadow = shadow_for(db)?;
    if let Some(hit) = shadow
        .memo
        .lock()
        .expect("shadow memo poisoned")
        .get(sql)
    {
        return Ok(hit.clone());
    }

    let analysis = {
        let conn = shadow.conn.lock().expect("shadow conn poisoned");
        derive(&conn, sql)?
    };

    if analysis.schema_change {
        // Keep the shadow's schema tracking the live one and drop every
        // memoized derivation computed against the old schema.
        let conn = shadow.conn.lock().expect("shadow conn poisoned");
        if let Err(err) = conn.execute_batch(sql) {
            tracing::warn!(db, %err, sql, "shadow: runtime DDL did not mirror");
        }
        drop(conn);
        shadow.memo.lock().expect("shadow memo poisoned").clear();
        return Ok(Arc::new(analysis));
    }

    let analysis = Arc::new(analysis);
    let mut memo = shadow.memo.lock().expect("shadow memo poisoned");
    if memo.len() < MEMO_CAP {
        memo.insert(Box::from(sql), analysis.clone());
    }
    Ok(analysis)
}

/// Owned copy of one authorizer callback — the hook is `Send + 'static`, so
/// it cannot borrow a local collector; it pushes owned actions through an
/// `Arc<Mutex<…>>` instead.
enum Collected {
    Read { table: Box<str>, column: Box<str> },
    Write { table: Box<str>, cols: Option<Box<str>> },
    Function { name: Box<str> },
    SchemaChange,
    Opaque,
    Neutral,
}

fn collect(ctx: &AuthContext<'_>) -> Collected {
    match ctx.action {
        AuthAction::Read {
            table_name,
            column_name,
        } => Collected::Read {
            table: Box::from(table_name),
            column: Box::from(column_name),
        },
        AuthAction::Insert { table_name } | AuthAction::Delete { table_name } => {
            Collected::Write {
                table: Box::from(table_name),
                cols: None,
            }
        }
        AuthAction::Update {
            table_name,
            column_name,
        } => Collected::Write {
            table: Box::from(table_name),
            cols: (!column_name.is_empty()).then(|| Box::from(column_name)),
        },
        AuthAction::Function { function_name } => Collected::Function {
            name: Box::from(function_name),
        },
        AuthAction::Select
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::Recursive
        | AuthAction::Analyze { .. }
        | AuthAction::Reindex { .. } => Collected::Neutral,
        AuthAction::CreateIndex { .. }
        | AuthAction::CreateTable { .. }
        | AuthAction::CreateTempIndex { .. }
        | AuthAction::CreateTempTable { .. }
        | AuthAction::CreateTempTrigger { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::CreateTrigger { .. }
        | AuthAction::CreateView { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::DropTempIndex { .. }
        | AuthAction::DropTempTable { .. }
        | AuthAction::DropTempTrigger { .. }
        | AuthAction::DropTempView { .. }
        | AuthAction::DropTrigger { .. }
        | AuthAction::DropView { .. }
        | AuthAction::AlterTable { .. }
        | AuthAction::CreateVtable { .. }
        | AuthAction::DropVtable { .. }
        | AuthAction::Attach { .. }
        | AuthAction::Detach { .. } => Collected::SchemaChange,
        // PRAGMA reads/writes engine state derivation can't model. Poison.
        AuthAction::Pragma { .. } => Collected::Function {
            name: Box::from("pragma"),
        },
        // rusqlite maps a NULL expected-string arg to Unknown; recover the
        // table for the two codes we rely on, else fail closed.
        AuthAction::Unknown { code, arg1, .. } => match (code, arg1) {
            (SQLITE_READ, Some(table)) => Collected::Read {
                table: Box::from(table),
                column: Box::from(""),
            },
            (SQLITE_UPDATE, Some(table)) => Collected::Write {
                table: Box::from(table),
                cols: None,
            },
            _ => Collected::Opaque,
        },
        // `AuthAction` is non_exhaustive: a variant added by a future
        // rusqlite is unattributable here — fail closed.
        _ => Collected::Opaque,
    }
}

/// Prepare `sql` on the shadow (NEVER `prepare_cached` — a cache hit fires
/// zero callbacks) with the authorizer installed, and interpret what fired.
fn derive(conn: &Connection, sql: &str) -> Result<SqlAnalysis, String> {
    let collected: Arc<Mutex<Vec<Collected>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = collected.clone();
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        sink.lock().expect("authorizer sink poisoned").push(collect(&ctx));
        Authorization::Allow
    }));
    let prepared = conn.prepare(sql);
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    prepared.map_err(|err| err.to_string())?;

    let mut analysis = SqlAnalysis::default();
    for action in collected.lock().expect("authorizer sink poisoned").drain(..) {
        match action {
            Collected::Read { table, column } => {
                analysis
                    .reads
                    .entry(table)
                    .or_insert_with(|| Cols::Named(HashSet::new()))
                    .add(&column);
            }
            Collected::Write { table, cols } => {
                let entry = analysis
                    .writes
                    .entry(table)
                    .or_insert_with(|| Cols::Named(HashSet::new()));
                match cols {
                    Some(column) => entry.add(&column),
                    None => entry.widen(),
                }
            }
            Collected::Function { name } => {
                if NONDETERMINISTIC_FNS.contains(&name.to_ascii_lowercase().as_str())
                    || &*name == "pragma"
                {
                    analysis.nondeterministic = true;
                }
            }
            Collected::SchemaChange => analysis.schema_change = true,
            Collected::Opaque => analysis.opaque = true,
            Collected::Neutral => {}
        }
    }
    Ok(analysis)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test uses its own db name: shadows are process-global.
    fn seed(db: &str) {
        mirror_schema(
            db,
            "CREATE TABLE arts (id INTEGER PRIMARY KEY, title TEXT, fav_count INTEGER NOT NULL DEFAULT 0);\
             CREATE TABLE fav (uid INTEGER, aid INTEGER, PRIMARY KEY (uid, aid));\
             CREATE TRIGGER trg_fav_ins AFTER INSERT ON fav \
             BEGIN UPDATE arts SET fav_count = fav_count + 1 WHERE id = NEW.aid; END;",
        );
    }

    fn named(cols: &[&str]) -> Cols {
        Cols::Named(cols.iter().map(|c| Box::from(*c)).collect())
    }

    /// a plain select reports exactly the columns it touches
    #[test]
    fn reads_report_exact_columns() {
        seed("t_reads");
        let a = analyze("t_reads", "SELECT title, fav_count FROM arts WHERE id = ?1").unwrap();
        assert_eq!(a.reads.get("arts"), Some(&named(&["title", "fav_count", "id"])));
        assert!(a.writes.is_empty() && !a.opaque && !a.nondeterministic && !a.schema_change);
    }

    /// an insert's write set carries the trigger-fired update, column-scoped
    #[test]
    fn trigger_expanded_write_set() {
        seed("t_trig");
        let a = analyze("t_trig", "INSERT INTO fav (uid, aid) VALUES (?1, ?2)").unwrap();
        assert_eq!(a.writes.get("fav"), Some(&Cols::All));
        assert_eq!(a.writes.get("arts"), Some(&named(&["fav_count"])));
    }

    /// an update is column-scoped to what it sets
    #[test]
    fn update_is_column_scoped() {
        seed("t_upd");
        let a = analyze("t_upd", "UPDATE arts SET title = ?1 WHERE id = ?2").unwrap();
        assert_eq!(a.writes.get("arts"), Some(&named(&["title"])));
    }

    /// count(*) touches the table with no column -> degrades to All
    #[test]
    fn count_star_degrades_to_all() {
        seed("t_count");
        let a = analyze("t_count", "SELECT count(*) FROM arts").unwrap();
        assert_eq!(a.reads.get("arts"), Some(&Cols::All));
        assert!(!a.opaque);
    }

    /// nondeterministic SQL functions poison the analysis
    #[test]
    fn nondeterministic_functions_poison() {
        seed("t_rand");
        assert!(analyze("t_rand", "SELECT random()").unwrap().nondeterministic);
        assert!(
            analyze("t_rand", "SELECT datetime('now')")
                .unwrap()
                .nondeterministic
        );
        assert!(
            !analyze("t_rand", "SELECT abs(-1)").unwrap().nondeterministic,
            "deterministic functions stay neutral"
        );
    }

    /// a statement the shadow can't prepare is an Err (and not memoized)
    #[test]
    fn failed_prepare_is_err() {
        seed("t_bad");
        assert!(analyze("t_bad", "SELECT nope FROM missing").is_err());
        assert!(analyze("t_bad", "SELECT nope FROM missing").is_err());
    }

    /// the memo returns the same Arc for a repeated text
    #[test]
    fn memoization_returns_same_arc() {
        seed("t_memo");
        let first = analyze("t_memo", "SELECT title FROM arts").unwrap();
        let second = analyze("t_memo", "SELECT title FROM arts").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    /// shadows are keyed per database name: same table name, different triggers
    #[test]
    fn per_db_shadows_are_isolated() {
        mirror_schema("t_iso_a", "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER);");
        mirror_schema(
            "t_iso_b",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER);\
             CREATE TABLE log (id INTEGER PRIMARY KEY, note TEXT);\
             CREATE TRIGGER trg AFTER INSERT ON t \
             BEGIN UPDATE log SET note = 'hit' WHERE id = 1; END;",
        );
        let plain = analyze("t_iso_a", "INSERT INTO t (n) VALUES (?1)").unwrap();
        let cascaded = analyze("t_iso_b", "INSERT INTO t (n) VALUES (?1)").unwrap();
        assert!(!plain.writes.contains_key("log"));
        assert_eq!(cascaded.writes.get("log"), Some(&named(&["note"])));
    }

    /// runtime DDL flags schema_change, applies to the shadow, flushes the memo
    #[test]
    fn runtime_ddl_updates_shadow_and_flushes() {
        mirror_schema(
            "t_ddl",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER);\
             CREATE TABLE audit (id INTEGER PRIMARY KEY, n INTEGER);",
        );
        let before = analyze("t_ddl", "INSERT INTO t (n) VALUES (?1)").unwrap();
        assert!(!before.writes.contains_key("audit"));
        let ddl = analyze(
            "t_ddl",
            "CREATE TRIGGER trg_audit AFTER INSERT ON t \
             BEGIN INSERT INTO audit (n) VALUES (NEW.n); END",
        )
        .unwrap();
        assert!(ddl.schema_change);
        let after = analyze("t_ddl", "INSERT INTO t (n) VALUES (?1)").unwrap();
        assert_eq!(after.writes.get("audit"), Some(&Cols::All));
    }

    /// with foreign_keys=ON, a cascading delete reports the child table write
    #[test]
    fn fk_cascade_reported() {
        mirror_schema(
            "t_fk",
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);\
             CREATE TABLE child (id INTEGER PRIMARY KEY, \
             pid INTEGER REFERENCES parent(id) ON DELETE CASCADE);",
        );
        let a = analyze("t_fk", "DELETE FROM parent WHERE id = ?1").unwrap();
        assert_eq!(a.writes.get("parent"), Some(&Cols::All));
        assert_eq!(a.writes.get("child"), Some(&Cols::All));
    }

    /// All intersects everything; Named intersects on shared columns only
    #[test]
    fn cols_intersection_semantics() {
        assert!(Cols::All.intersects(&named(&["x"])));
        assert!(named(&["x"]).intersects(&Cols::All));
        assert!(named(&["x", "y"]).intersects(&named(&["y"])));
        assert!(!named(&["x"]).intersects(&named(&["y"])));
    }
}
