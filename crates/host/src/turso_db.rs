//! Turso DB backend (`--features turso`): the turso counterpart to `cursor.rs`,
//! mounted as `crate::cursor`. Provides the same public surface — the cursor
//! read FFI (`roc_sql_*`), the ctx sweep, and the write-effect entry points
//! (`run_write_txn` / `run_autocommit`) — so the rest of the host is unchanged.
//!
//! Turso's statement is owned (`Box<TursoStatement>`, no borrowed lifetime), so
//! a cursor is a flat struct rather than rusqlite's three-level self-reference.
//! The synchronous stepping mode (`async_io: false`) makes step/execute block
//! instead of returning `Io`, matching the blocking cursor model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use turso_sdk_kit::rsapi::{
    TursoConnection, TursoDatabase, TursoDatabaseConfig, TursoError, TursoStatement,
    TursoStatusCode, Value,
};

use crate::abi;
use crate::db::{DbError, SqlValue, Write};
use crate::effects::decref_list_with;
use crate::roc_call::roc_str;
use crate::roc_host;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Connection pool: one opened database per path (WAL set once); connect on
// checkout. Turso connections are cheap Arc handles onto the shared database.
// ---------------------------------------------------------------------------

fn databases() -> &'static Mutex<HashMap<String, Arc<TursoDatabase>>> {
    static DBS: OnceLock<Mutex<HashMap<String, Arc<TursoDatabase>>>> = OnceLock::new();
    DBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn open_database(path: &str) -> Result<Arc<TursoDatabase>, String> {
    let mut dbs = databases().lock().map_err(|_| "turso db registry poisoned".to_string())?;
    if let Some(db) = dbs.get(path) {
        return Ok(db.clone());
    }
    let db = TursoDatabase::new(TursoDatabaseConfig {
        path: path.to_string(),
        experimental_features: None,
        async_io: false,
        encryption: None,
        vfs: None,
        io: None,
        db_file: None,
    });
    let io = db.open().map_err(|e| format!("cannot open database {path:?}: {e:?}"))?;
    if io.is_io() {
        return Err(format!("database {path:?} returned Io under async_io=false"));
    }
    // Establish WAL once on a lone connection before it is shared.
    let conn = db.connect().map_err(|e| format!("cannot connect to {path:?}: {e:?}"))?;
    exec_raw(&conn, "PRAGMA journal_mode=WAL").map_err(|e| format!("WAL on {path:?}: {e:?}"))?;
    let _ = conn.close();
    dbs.insert(path.to_owned(), db.clone());
    Ok(db)
}

/// Max idle connections retained per database path. Outstanding connections are
/// bounded by in-flight request concurrency; returning past this cap just drops
/// the connection (a cheap Arc handle) rather than growing the idle set.
/// Mirrors rusqlite's `SQLITE_POOL_MAX`.
const POOL_MAX: usize = 16;

fn pools() -> &'static Mutex<HashMap<String, Vec<Arc<TursoConnection>>>> {
    static POOLS: OnceLock<Mutex<HashMap<String, Vec<Arc<TursoConnection>>>>> = OnceLock::new();
    POOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A connection borrowed from the pool for one request. On drop it returns
/// itself to the pool's idle set so its prepared-statement cache
/// (`prepare_cached`, keyed per connection) stays warm across requests — the
/// whole point of pooling here. A panic unwinding past this guard across a Roc
/// frame skips the drop (see `roc_call.rs`); the connection is then simply not
/// returned — leaked to a plain drop, never shared while in use — and the pool
/// re-creates on the next checkout.
pub(crate) struct PooledConn {
    conn: Option<Arc<TursoConnection>>,
    path: String,
}

impl PooledConn {
    fn conn(&self) -> &TursoConnection {
        self.conn.as_ref().expect("pooled connection already returned")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take()
            && let Ok(mut pools) = pools().lock()
        {
            let idle = pools.entry(std::mem::take(&mut self.path)).or_default();
            if idle.len() < POOL_MAX {
                idle.push(conn);
            }
        }
    }
}

/// Check out a connection for `db`: reuse an idle one (warm statement cache) or
/// open a fresh one. Returned to the pool when the `PooledConn` drops.
fn checkout(path: &str) -> Result<PooledConn, String> {
    {
        let mut pools = pools().lock().map_err(|_| "turso pool poisoned".to_string())?;
        if let Some(idle) = pools.get_mut(path)
            && let Some(conn) = idle.pop()
        {
            return Ok(PooledConn { conn: Some(conn), path: path.to_owned() });
        }
    }
    let db = open_database(path)?;
    let conn = db.connect().map_err(|e| format!("turso checkout for {path:?}: {e:?}"))?;
    conn.set_busy_timeout(BUSY_TIMEOUT);
    Ok(PooledConn { conn: Some(conn), path: path.to_owned() })
}

/// Convert an engine-neutral bound param to a turso value.
fn to_turso(value: &SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Int(n) => Value::from_i64(*n),
        SqlValue::Real(f) => Value::from_f64(*f),
        SqlValue::Text(s) => Value::build_text(s.clone()),
        SqlValue::Blob(b) => Value::Blob(b.clone()),
    }
}

/// Prepare + bind + run a statement to completion (no result rows expected).
fn exec_write(conn: &TursoConnection, sql: &str, params: &[SqlValue]) -> Result<(), TursoError> {
    let mut stmt = conn.prepare_cached(sql)?;
    for (i, value) in params.iter().enumerate() {
        stmt.bind_positional(i + 1, to_turso(value))?;
    }
    stmt.execute(None)?;
    Ok(())
}

/// Run a parameter-less statement (pragmas, BEGIN/COMMIT/ROLLBACK).
fn exec_raw(conn: &TursoConnection, sql: &str) -> Result<(), TursoError> {
    let mut stmt = conn.prepare_cached(sql)?;
    stmt.execute(None)?;
    Ok(())
}

/// Map a turso error to the engine-neutral `DbError`. Constraint messages are
/// column-qualified ("UNIQUE constraint failed: users.email (19)"); FOREIGN KEY
/// violations name the constraint, so message inspection classifies them.
fn classify(err: &TursoError) -> DbError {
    match err {
        TursoError::Busy(_) | TursoError::BusySnapshot(_) => DbError::Busy,
        TursoError::Constraint(msg) => {
            if msg.to_uppercase().contains("FOREIGN KEY") {
                DbError::ForeignKeyViolation(msg.clone())
            } else {
                // UNIQUE and PRIMARY KEY both surface as UNIQUE-constraint text.
                DbError::UniqueViolation(msg.clone())
            }
        }
        other => DbError::Other(format!("{other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Cursor: an owned statement plus the connection keeping it alive.
// ---------------------------------------------------------------------------

struct Cursor {
    columns: Vec<String>,
    stmt: Box<TursoStatement>,
    /// Held so the connection outlives the statement, and returned to the pool
    /// when the cursor is finalized. Declared AFTER `stmt` so the statement
    /// drops (finalizes) first — the connection must not re-enter the pool
    /// (available to another thread) while its statement is still live.
    _conn: PooledConn,
    /// True after a step that produced a row; getters read the current row.
    has_row: bool,
}

fn registry() -> &'static Mutex<HashMap<u64, u64>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// See `cursor.rs` for the non-escaping-handle contract this relies on: a handle
/// in use always addresses a live, single-thread-owned cursor.
fn with_cursor<R>(handle: u64, f: impl FnOnce(&mut Cursor) -> R) -> Option<R> {
    if handle == 0 {
        return None;
    }
    Some(f(unsafe { &mut *(handle as *mut Cursor) }))
}

pub fn sweep_ctx_cursors(ctx: u64) -> usize {
    let orphaned: Vec<u64> = {
        let mut reg = registry().lock().expect("cursor registry poisoned");
        let handles: Vec<u64> = reg.iter().filter(|(_, c)| **c == ctx).map(|(h, _)| *h).collect();
        for h in &handles {
            reg.remove(h);
        }
        handles
    };
    let count = orphaned.len();
    if count > 0 {
        tracing::warn!("ctx {ctx}: swept {count} unfinished cursor(s)");
    }
    for handle in orphaned {
        drop(unsafe { Box::from_raw(handle as *mut Cursor) });
    }
    count
}

#[allow(dead_code)]
pub fn live_cursor_count() -> u64 {
    registry().lock().expect("cursor registry poisoned").len() as u64
}

fn begin_ok(handle: u64) -> abi::HostSqlBeginResult {
    abi::HostSqlBeginResult {
        tag: abi::HostSqlBeginResultTag::Ok,
        payload: abi::HostSqlBeginResultPayload { ok: std::mem::ManuallyDrop::new(handle) },
    }
}

fn begin_err(message: &str) -> abi::HostSqlBeginResult {
    abi::HostSqlBeginResult {
        tag: abi::HostSqlBeginResultTag::Err,
        payload: abi::HostSqlBeginResultPayload { err: std::mem::ManuallyDrop::new(roc_str(message)) },
    }
}

fn step_err(message: &str) -> abi::HostSqlStepResult {
    abi::HostSqlStepResult {
        tag: abi::HostSqlStepResultTag::Err,
        payload: abi::HostSqlStepResultPayload { err: std::mem::ManuallyDrop::new(roc_str(message)) },
    }
}

fn step_ok(has_row: bool) -> abi::HostSqlStepResult {
    abi::HostSqlStepResult {
        tag: abi::HostSqlStepResultTag::Ok,
        payload: abi::HostSqlStepResultPayload { ok: std::mem::ManuallyDrop::new(has_row) },
    }
}

/// `Host.sql_begin! : { ctx, db, sql, params } => Try(U64, Str)`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_begin(args: abi::HostSqlBeginArgs) -> abi::HostSqlBeginResult {
    let _t = crate::profile::exec();
    let host = roc_host();
    let ctx = args.ctx;
    let db = String::from_utf8_lossy(args.db.as_slice()).into_owned();
    let sql = String::from_utf8_lossy(args.sql.as_slice()).into_owned();
    let params: Vec<SqlValue> =
        args.params.as_slice().iter().map(crate::db::param_from_host).collect();
    unsafe {
        args.db.decref(host);
        decref_list_with(args.params, |value| value.decref(host));
        args.sql.decref(host);
    }

    let pooled = match checkout(&db) {
        Ok(pooled) => pooled,
        Err(message) => return begin_err(&message),
    };
    match build_cursor(pooled, &sql, &params) {
        Ok(cursor) => {
            let handle = Box::into_raw(Box::new(cursor)) as u64;
            registry().lock().expect("cursor registry poisoned").insert(handle, ctx);
            begin_ok(handle)
        }
        Err(message) => begin_err(&message),
    }
}

fn build_cursor(pooled: PooledConn, sql: &str, params: &[SqlValue]) -> Result<Cursor, String> {
    // prepare_cached keys the compiled Program by SQL string on the connection;
    // a hit skips parse+plan (measured ~+11µs/query otherwise). The connection
    // comes from the pool and is reused across requests, so the cache persists,
    // and the app's SQL-string set is bounded (LIMIT/OFFSET are `?` params, not
    // in the text). Mirrors cursor.rs's rusqlite `prepare_cached`.
    let mut stmt = pooled.conn().prepare_cached(sql).map_err(|e| format!("{e:?}"))?;
    for (i, value) in params.iter().enumerate() {
        stmt.bind_positional(i + 1, to_turso(value)).map_err(|e| format!("bind param {}: {e:?}", i + 1))?;
    }
    let columns: Vec<String> = (0..stmt.column_count())
        .map(|i| stmt.column_name(i).unwrap_or_default())
        .collect();
    Ok(Cursor { columns, stmt, _conn: pooled, has_row: false })
}

/// `Host.sql_step! : U64 => Try(Bool, Str)`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_step(handle: u64) -> abi::HostSqlStepResult {
    let _t = crate::profile::exec();
    match with_cursor(handle, |cursor| match cursor.stmt.step(None) {
        Ok(TursoStatusCode::Row) => {
            cursor.has_row = true;
            step_ok(true)
        }
        Ok(TursoStatusCode::Done) => {
            cursor.has_row = false;
            step_ok(false)
        }
        Ok(TursoStatusCode::Io) => {
            cursor.has_row = false;
            step_err("turso returned Io under async_io=false")
        }
        Err(err) => {
            cursor.has_row = false;
            step_err(&format!("{err:?}"))
        }
    }) {
        Some(result) => result,
        None => step_err("sql_step! on an unknown cursor handle"),
    }
}

/// Read the current row's column `index` as an owned turso value, if a row is live.
fn with_value<R>(handle: u64, index: u64, f: impl FnOnce(Value) -> R) -> Option<R> {
    with_cursor(handle, |cursor| {
        if !cursor.has_row {
            return None;
        }
        cursor.stmt.row_value(index as usize).ok().map(f)
    })
    .flatten()
}

/// `Host.sql_columns! : U64 => List(Str)`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_columns(handle: u64) -> abi::RocList<abi::RocStr> {
    let names = with_cursor(handle, |cursor| cursor.columns.clone()).unwrap_or_default();
    let host = roc_host();
    unsafe {
        let list = abi::RocList::<abi::RocStr>::allocate(names.len(), host);
        for (index, name) in names.iter().enumerate() {
            list.elements.add(index).write(roc_str(name));
        }
        list
    }
}

/// `Host.sql_col_type! : U64, U64 => Host.ColType`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_type(handle: u64, index: u64) -> abi::HostColType {
    crate::profile::getter();
    with_value(handle, index, |v| match &v {
        Value::Null => abi::HostColType::Null,
        Value::Text(_) => abi::HostColType::Text,
        Value::Blob(_) => abi::HostColType::Blob,
        // `as_int` is `Some` only for an integer; a float returns `None`.
        Value::Numeric(_) if v.as_int().is_some() => abi::HostColType::Integer,
        Value::Numeric(_) => abi::HostColType::Real,
    })
    .unwrap_or(abi::HostColType::Null)
}

/// `Host.sql_col_i64! : U64, U64 => I64`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_i64(handle: u64, index: u64) -> i64 {
    crate::profile::getter();
    with_value(handle, index, |v| match &v {
        Value::Numeric(_) => v.to_float_or_zero() as i64,
        Value::Text(_) => v.to_text().and_then(|s| s.parse().ok()).unwrap_or(0),
        _ => 0,
    })
    .unwrap_or(0)
}

/// `Host.sql_col_f64! : U64, U64 => F64`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_f64(handle: u64, index: u64) -> f64 {
    crate::profile::getter();
    with_value(handle, index, |v| match &v {
        Value::Numeric(_) => v.to_float_or_zero(),
        Value::Text(_) => v.to_text().and_then(|s| s.parse().ok()).unwrap_or(0.0),
        _ => 0.0,
    })
    .unwrap_or(0.0)
}

/// `Host.sql_col_str! : U64, U64 => Str`. One copy from the borrowed cell bytes.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_str(handle: u64, index: u64) -> abi::RocStr {
    crate::profile::getter();
    let host = roc_host();
    with_value(handle, index, |v| match &v {
        Value::Text(_) => abi::RocStr::from_slice(v.to_text().unwrap_or("").as_bytes(), host),
        Value::Blob(b) => abi::RocStr::from_slice(b, host),
        Value::Numeric(_) => {
            if let Some(i) = v.as_int() {
                abi::RocStr::from_slice(i.to_string().as_bytes(), host)
            } else {
                abi::RocStr::from_slice(v.to_float_or_zero().to_string().as_bytes(), host)
            }
        }
        Value::Null => abi::RocStr::from_slice(&[], host),
    })
    .unwrap_or_else(|| abi::RocStr::from_slice(&[], host))
}

/// `Host.sql_col_blob! : U64, U64 => List(U8)`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_blob(handle: u64, index: u64) -> abi::RocListWith<u8, false> {
    crate::profile::getter();
    let host = roc_host();
    with_value(handle, index, |v| {
        let bytes: &[u8] = match &v {
            Value::Blob(b) => b,
            Value::Text(_) => v.to_text().unwrap_or("").as_bytes(),
            _ => &[],
        };
        unsafe { abi::RocListWith::<u8, false>::from_slice(bytes, host) }
    })
    .unwrap_or_else(|| unsafe { abi::RocListWith::<u8, false>::from_slice(&[], host) })
}

/// `Host.sql_finish! : U64 => {}`. Idempotent.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_finish(handle: u64) {
    let _t = crate::profile::exec();
    let present = {
        let mut reg = registry().lock().expect("cursor registry poisoned");
        reg.remove(&handle).is_some()
    };
    if present {
        drop(unsafe { Box::from_raw(handle as *mut Cursor) });
    }
}

// ---------------------------------------------------------------------------
// Write effects.
// ---------------------------------------------------------------------------

/// Run `writes` in one `BEGIN IMMEDIATE` transaction, in order.
pub fn run_write_txn(db: &str, writes: &[Write]) -> Result<(), (usize, DbError)> {
    let pooled = checkout(db).map_err(|message| (0, DbError::Other(message)))?;
    let conn = pooled.conn();
    exec_raw(conn, "BEGIN IMMEDIATE").map_err(|err| (0, classify(&err)))?;
    for (index, (sql, params)) in writes.iter().enumerate() {
        if let Err(err) = exec_write(conn, sql, params) {
            let _ = exec_raw(conn, "ROLLBACK");
            return Err((index, classify(&err)));
        }
    }
    let last = writes.len().saturating_sub(1);
    if let Err(err) = exec_raw(conn, "COMMIT") {
        let _ = exec_raw(conn, "ROLLBACK");
        return Err((last, classify(&err)));
    }
    Ok(())
}

/// Run one write outside a transaction (autocommit) — the `Opt` DbWrite path.
pub fn run_autocommit(db: &str, sql: &str, params: &[SqlValue]) -> Result<(), DbError> {
    let pooled = checkout(db).map_err(DbError::Other)?;
    exec_write(pooled.conn(), sql, params).map_err(|err| classify(&err))
}
