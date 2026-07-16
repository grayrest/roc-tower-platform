//! Streaming SQLite cursors for the typed-SQL FFI (`Host.sql_begin!` … etc).
//!
//! A cursor holds a pooled connection checked out and a live statement stepping
//! over it, so Roc pulls rows and cells one at a time with no intermediate
//! `List(List(SqlValue))` materialization.
//!
//! ## Statement caching
//!
//! The statement comes from `Connection::prepare_cached`, so a repeated query
//! reuses its compiled program from the connection's 64-entry cache instead of
//! recompiling (article_select alone has four correlated subqueries — a fresh
//! prepare each request is ~2x the whole query cost; measured in the Phase 2
//! microbench). rusqlite's `CachedStatement<'conn>` borrows its `Connection`
//! and `Rows<'stmt>` borrows the statement, so a suspended cursor is a
//! three-level self-reference. We box each level on the heap and hold the lower
//! levels by raw pointer with erased lifetimes; the boxes never move and are
//! dropped in reverse (rows → statement → connection), so the borrows stay
//! valid and the statement resets back into the cache before its connection
//! returns to the pool.
//!
//! ## Lifetime & safety
//!
//! Each handle is minted by `sql_begin!` and used only by its owning request's
//! single blocking thread. The registry maps handle → `Box<Cursor>` raw
//! pointer; the heap `Cursor` has a stable address across map rehashes.
//! Step/getter calls copy the raw pointer out under the registry lock and
//! release the lock BEFORE touching SQLite, so a slow step (up to the 5s busy
//! timeout) never stalls other requests' cursor ops. That is sound because:
//!   * a handle is never shared between requests (unique ids, no forging in
//!     generated/`Sql.roc` code), so no two threads alias one `Cursor`;
//!   * teardown that could free a `Cursor` — `sql_finish!` or the per-ctx sweep
//!     — is driven by the owning request, and the router's reaper defers the
//!     sweep until the handler's blocking task has returned (plan decision 6),
//!     so no free races a live step.
//!
//! `SQLITE_THREADSAFE=1` (bundled default) additionally serializes calls that
//! touch the same connection.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use rusqlite::types::ValueRef;
use rusqlite::{CachedStatement, Row, Rows};

use rusqlite::TransactionBehavior;

use crate::abi;
use crate::db::{DbError, SqlValue, Write};
use crate::effects::{checkout_sqlite, decref_list_with, to_rusqlite, PooledSqlite};
use crate::roc_call::roc_str;
use crate::roc_host;

/// A suspended statement plus the connection and row iterator it borrows.
/// The three pointers form a self-reference (rows → stmt → conn); they are
/// dropped in that order and the heap boxes never move.
struct Cursor {
    /// Column names, cached once at begin for by-name decode in `Sql.roc`.
    columns: Vec<String>,
    /// The current row after a successful `step`, or null before the first
    /// step / after `Done`. Borrows `rows`; replaced on each step.
    current: *const Row<'static>,
    rows: *mut Rows<'static>,
    stmt: *mut CachedStatement<'static>,
    conn: *mut PooledSqlite,
}

impl Drop for Cursor {
    fn drop(&mut self) {
        // Reverse construction order so each borrow outlives its borrower:
        // rows (borrows stmt) → stmt (resets into the connection cache) → conn
        // (returns to the pool).
        unsafe {
            drop(Box::from_raw(self.rows));
            drop(Box::from_raw(self.stmt));
            drop(Box::from_raw(self.conn));
        }
    }
}

/// The cursor handle IS the heap `Cursor` pointer (as a u64), so step/getter
/// calls dereference it directly with no registry lookup or lock on the hot
/// path — the per-cell getters are the whole point of the cursor design, and a
/// per-call `Mutex`+`HashMap` there erased the win (measured). This is sound
/// because handles are non-escaping (plan decision 2): `Sql.query!` and
/// generated readers open, step, and finish a cursor within one function frame
/// and never hand the handle to app code, so a handle in use always addresses a
/// live cursor.
///
/// The registry maps live handle → ctx and is touched ONLY at begin / finish /
/// sweep (cold paths) — it exists so the router's reaper can finalize cursors a
/// handler leaked on an error path, and so finish/unknown-handle stay safe
/// no-ops instead of freeing an arbitrary pointer.
fn registry() -> &'static Mutex<HashMap<u64, u64>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `f` against the cursor for `handle`. On the hot path the handle is cast
/// straight to the `Cursor` pointer — no lock. None only for the null handle.
fn with_cursor<R>(handle: u64, f: impl FnOnce(&mut Cursor) -> R) -> Option<R> {
    if handle == 0 {
        return None;
    }
    // SAFETY: per the non-escaping-handle contract above, a handle passed to a
    // getter/step addresses a live, exclusively-owned Cursor.
    Some(f(unsafe { &mut *(handle as *mut Cursor) }))
}

/// Sweep every cursor left open by a torn-down request ctx. Called from the
/// ctx teardown guard (deferred past the handler by the router's reaper), so it
/// never races a live step. Returns how many cursors it finalized.
pub fn sweep_ctx_cursors(ctx: u64) -> usize {
    let orphaned: Vec<u64> = {
        let mut reg = registry().lock().expect("cursor registry poisoned");
        let handles: Vec<u64> =
            reg.iter().filter(|(_, c)| **c == ctx).map(|(h, _)| *h).collect();
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
        // SAFETY: the handle was a live registry entry; ownership transfers
        // back and drop finalizes.
        drop(unsafe { Box::from_raw(handle as *mut Cursor) });
    }
    count
}

/// Live cursor count across all requests (debug / leak assertions).
#[allow(dead_code)]
pub fn live_cursor_count() -> u64 {
    registry().lock().expect("cursor registry poisoned").len() as u64
}

fn begin_ok(handle: u64) -> abi::HostSqlBeginResult {
    abi::HostSqlBeginResult {
        tag: abi::HostSqlBeginResultTag::Ok,
        payload: abi::HostSqlBeginResultPayload {
            ok: std::mem::ManuallyDrop::new(handle),
        },
    }
}

fn begin_err(message: &str) -> abi::HostSqlBeginResult {
    abi::HostSqlBeginResult {
        tag: abi::HostSqlBeginResultTag::Err,
        payload: abi::HostSqlBeginResultPayload {
            err: std::mem::ManuallyDrop::new(roc_str(message)),
        },
    }
}

fn step_err(message: &str) -> abi::HostSqlStepResult {
    abi::HostSqlStepResult {
        tag: abi::HostSqlStepResultTag::Err,
        payload: abi::HostSqlStepResultPayload {
            err: std::mem::ManuallyDrop::new(roc_str(message)),
        },
    }
}

fn step_ok(has_row: bool) -> abi::HostSqlStepResult {
    abi::HostSqlStepResult {
        tag: abi::HostSqlStepResultTag::Ok,
        payload: abi::HostSqlStepResultPayload {
            ok: std::mem::ManuallyDrop::new(has_row),
        },
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

    // Response-cache attribution. Init-time (ctx 0) DDL mirrors the schema
    // into the analysis shadow; request-time statements record their derived
    // read set onto the ctx, failing CLOSED (poison) when underivable.
    if ctx == 0 {
        if crate::shadow::looks_like_ddl(&sql) {
            crate::shadow::mirror_schema(&db, &sql);
        }
    } else if crate::cache::global().is_some() {
        match crate::shadow::analyze(&db, &sql) {
            Ok(analysis) => crate::effects::record_read(ctx, &analysis),
            Err(err) => {
                tracing::debug!(%err, sql, "sql_begin: underivable read; response uncacheable");
                crate::effects::poison_ctx(ctx);
            }
        }
    }

    let conn = match checkout_sqlite(&db) {
        Ok(conn) => conn,
        Err(message) => return begin_err(&message),
    };

    match build_cursor(conn, &sql, &params) {
        Ok(cursor) => {
            let handle = Box::into_raw(Box::new(cursor)) as u64;
            registry()
                .lock()
                .expect("cursor registry poisoned")
                .insert(handle, ctx);
            begin_ok(handle)
        }
        Err(message) => begin_err(&message),
    }
}

/// Assemble the self-referential cursor: box the connection, prepare a cached
/// statement borrowing it, bind params, then box the row iterator borrowing the
/// statement. On any error the partially built boxes are freed in order.
fn build_cursor(
    conn: PooledSqlite,
    sql: &str,
    params: &[SqlValue],
) -> Result<Cursor, String> {
    let conn: *mut PooledSqlite = Box::into_raw(Box::new(conn));
    // SAFETY: `conn` is a stable heap box that outlives the statement and rows
    // and is never moved; extend its borrow to 'static.
    let conn_ref: &'static PooledSqlite = unsafe { &*conn };

    let mut stmt: CachedStatement<'static> = match conn_ref.prepare_cached(sql) {
        Ok(s) => s,
        Err(err) => {
            drop(unsafe { Box::from_raw(conn) });
            return Err(err.to_string());
        }
    };
    for (i, value) in params.iter().enumerate() {
        if let Err(err) = stmt.raw_bind_parameter(i + 1, to_rusqlite(value)) {
            drop(stmt);
            drop(unsafe { Box::from_raw(conn) });
            return Err(format!("bind param {}: {err}", i + 1));
        }
    }
    let columns: Vec<String> = stmt.column_names().iter().map(|s| (*s).to_owned()).collect();

    let stmt: *mut CachedStatement<'static> = Box::into_raw(Box::new(stmt));
    // SAFETY: stable heap box, never moved, dropped after `rows`.
    let stmt_ref: &'static mut CachedStatement<'static> = unsafe { &mut *stmt };
    let rows: *mut Rows<'static> = Box::into_raw(Box::new(stmt_ref.raw_query()));

    Ok(Cursor {
        columns,
        current: std::ptr::null(),
        rows,
        stmt,
        conn,
    })
}

/// `Host.sql_step! : U64 => Try(Bool, Str)`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_step(handle: u64) -> abi::HostSqlStepResult {
    let _t = crate::profile::exec();
    match with_cursor(handle, |cursor| {
        // SAFETY: `rows` borrows the still-live statement; the returned row
        // reference is valid until the next step, which is exactly when we
        // replace `current`.
        let rows = unsafe { &mut *cursor.rows };
        match rows.next() {
            Ok(Some(row)) => {
                cursor.current = row as *const Row<'static>;
                step_ok(true)
            }
            Ok(None) => {
                cursor.current = std::ptr::null();
                step_ok(false)
            }
            Err(err) => {
                cursor.current = std::ptr::null();
                step_err(&err.to_string())
            }
        }
    }) {
        Some(result) => result,
        None => step_err("sql_step! on an unknown cursor handle"),
    }
}

/// The current row, or None before the first step / after Done / unknown handle.
fn with_row<R>(handle: u64, f: impl FnOnce(&Row<'static>) -> R) -> Option<R> {
    with_cursor(handle, |cursor| {
        if cursor.current.is_null() {
            None
        } else {
            // SAFETY: non-null `current` was set by the last step and is valid
            // until the next step on this single-threaded cursor.
            Some(f(unsafe { &*cursor.current }))
        }
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
    with_row(handle, |row| match row.get_ref(index as usize) {
        Ok(ValueRef::Integer(_)) => abi::HostColType::Integer,
        Ok(ValueRef::Real(_)) => abi::HostColType::Real,
        Ok(ValueRef::Text(_)) => abi::HostColType::Text,
        Ok(ValueRef::Blob(_)) => abi::HostColType::Blob,
        _ => abi::HostColType::Null,
    })
    .unwrap_or(abi::HostColType::Null)
}

/// `Host.sql_col_i64! : U64, U64 => I64`. Coerces per SQLite affinity; NULL and
/// non-numeric read as 0 (callers check `sql_col_type!` when NULL matters).
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_i64(handle: u64, index: u64) -> i64 {
    crate::profile::getter();
    with_row(handle, |row| match row.get_ref(index as usize) {
        Ok(ValueRef::Integer(n)) => n,
        Ok(ValueRef::Real(f)) => f as i64,
        Ok(ValueRef::Text(b)) => std::str::from_utf8(b).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        _ => 0,
    })
    .unwrap_or(0)
}

/// `Host.sql_col_f64! : U64, U64 => F64`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_f64(handle: u64, index: u64) -> f64 {
    crate::profile::getter();
    with_row(handle, |row| match row.get_ref(index as usize) {
        Ok(ValueRef::Real(f)) => f,
        Ok(ValueRef::Integer(n)) => n as f64,
        Ok(ValueRef::Text(b)) => std::str::from_utf8(b).ok().and_then(|s| s.parse().ok()).unwrap_or(0.0),
        _ => 0.0,
    })
    .unwrap_or(0.0)
}

/// `Host.sql_col_str! : U64, U64 => Str`. NULL and non-text coerce (empty for
/// NULL); callers that must distinguish NULL check `sql_col_type!` first.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_str(handle: u64, index: u64) -> abi::RocStr {
    crate::profile::getter();
    // Build the RocStr directly from the borrowed cell bytes inside the closure
    // — the common Text/Blob path is then a single copy into Roc-owned memory,
    // with no intermediate `Vec`. (Integer/Real coerce through a `to_string`.)
    let host = roc_host();
    with_row(handle, |row| match row.get_ref(index as usize) {
        Ok(ValueRef::Text(b)) | Ok(ValueRef::Blob(b)) => abi::RocStr::from_slice(b, host),
        Ok(ValueRef::Integer(n)) => abi::RocStr::from_slice(n.to_string().as_bytes(), host),
        Ok(ValueRef::Real(f)) => abi::RocStr::from_slice(f.to_string().as_bytes(), host),
        _ => abi::RocStr::from_slice(&[], host),
    })
    .unwrap_or_else(|| abi::RocStr::from_slice(&[], host))
}

/// `Host.sql_col_blob! : U64, U64 => List(U8)`.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_col_blob(handle: u64, index: u64) -> abi::RocListWith<u8, false> {
    crate::profile::getter();
    let host = roc_host();
    with_row(handle, |row| {
        let bytes: &[u8] = match row.get_ref(index as usize) {
            Ok(ValueRef::Blob(b)) | Ok(ValueRef::Text(b)) => b,
            _ => &[],
        };
        unsafe { abi::RocListWith::<u8, false>::from_slice(bytes, host) }
    })
    .unwrap_or_else(|| unsafe { abi::RocListWith::<u8, false>::from_slice(&[], host) })
}

/// `Host.sql_finish! : U64 => {}`. Idempotent: only a handle still in the
/// registry is freed, so a double finish or an unknown handle is a safe no-op
/// (rather than freeing an arbitrary pointer).
#[unsafe(no_mangle)]
pub extern "C" fn roc_sql_finish(handle: u64) {
    let _t = crate::profile::exec();
    let present = {
        let mut reg = registry().lock().expect("cursor registry poisoned");
        reg.remove(&handle).is_some()
    };
    if present {
        // SAFETY: the handle was a live registry entry; ownership transfers
        // back and drop finalizes.
        drop(unsafe { Box::from_raw(handle as *mut Cursor) });
    }
}

// ---------------------------------------------------------------------------
// Write effects (the rusqlite backend's half of the effect executor). Both DB
// backends expose `run_write_txn` + `run_autocommit`, so the effect layer stays
// engine-agnostic.
// ---------------------------------------------------------------------------

/// Run `writes` in one `BEGIN IMMEDIATE` transaction, in order. On the first
/// failure the whole group rolls back and `Err((index, error))` names it.
pub fn run_write_txn(db: &str, writes: &[Write]) -> Result<(), (usize, DbError)> {
    let mut connection = checkout_sqlite(db).map_err(|message| (0, DbError::Other(message)))?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| (0, classify(&err)))?;
    for (index, (sql, params)) in writes.iter().enumerate() {
        let bound: Vec<rusqlite::types::Value> = params.iter().map(to_rusqlite).collect();
        let outcome = tx
            .prepare_cached(sql)
            .and_then(|mut statement| statement.execute(rusqlite::params_from_iter(bound.iter())));
        if let Err(err) = outcome {
            // Dropping `tx` without commit rolls the whole group back.
            return Err((index, classify(&err)));
        }
    }
    let last = writes.len().saturating_sub(1);
    tx.commit().map_err(|err| (last, classify(&err)))
}

/// Reserve the next monotonic id for `db` — the persistent host-side sequence
/// backing `Req.reserved_id!`. Bumps a per-database `counters` row and returns
/// the new value; lazily creates the sequence table on the first reservation
/// against a fresh database. Runs only on writes (reservations), never on a
/// read; the `counters` table is host-owned and read by nothing else.
pub fn reserve_next_id(db: &str) -> Result<i64, String> {
    const BUMP: &str = "UPDATE counters SET v = v + 1 WHERE name = 'reserved_id' RETURNING v";
    let conn = checkout_sqlite(db)?;
    // Fast path: after the first reservation the row always exists.
    if let Ok(v) = conn.query_row(BUMP, [], |row| row.get::<_, i64>(0)) {
        return Ok(v);
    }
    // Cold path (fresh database): create the sequence table and seed its row.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, v INTEGER NOT NULL);\
         INSERT INTO counters (name, v) VALUES ('reserved_id', 0) ON CONFLICT(name) DO NOTHING;",
    )
    .map_err(|err| err.to_string())?;
    conn.query_row(BUMP, [], |row| row.get::<_, i64>(0))
        .map_err(|err| err.to_string())
}

/// Run one write outside a transaction (autocommit) — the `Opt` DbWrite path.
pub fn run_autocommit(db: &str, sql: &str, params: &[SqlValue]) -> Result<(), DbError> {
    let connection = checkout_sqlite(db).map_err(DbError::Other)?;
    let bound: Vec<rusqlite::types::Value> = params.iter().map(to_rusqlite).collect();
    connection
        .prepare_cached(sql)
        .and_then(|mut statement| statement.execute(rusqlite::params_from_iter(bound.iter())))
        .map(|_| ())
        .map_err(|err| classify(&err))
}

/// Map a rusqlite error to the engine-neutral `DbError`.
fn classify(err: &rusqlite::Error) -> DbError {
    if let rusqlite::Error::SqliteFailure(inner, message) = err {
        let detail = message.clone().unwrap_or_else(|| err.to_string());
        match inner.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
                return DbError::UniqueViolation(detail);
            }
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                return DbError::ForeignKeyViolation(detail);
            }
            _ => {}
        }
        if matches!(
            inner.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ) {
            return DbError::Busy;
        }
        return DbError::Other(detail);
    }
    DbError::Other(err.to_string())
}
