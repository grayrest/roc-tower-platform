//! Host-side effect executor (Arch B).
//!
//! A pure handler returns a `Response` carrying a list of `Effect`s. The
//! critical `Must` effects run inside the handler's `spawn_blocking` thread
//! (spike S2), after the handler returns and before the `ResponseParts` are
//! synthesized; the fire-and-forget `Opt` effects are deferred to after the
//! response is flushed (B3):
//!
//!   1. DB `Must` writes run FIRST, in order, in one `BEGIN IMMEDIATE`
//!      transaction on a pooled connection — atomic, so any failure rolls the
//!      whole group back.
//!   2. External `Must` writes (HttpSend, Log) run AFTER the commit.
//!   3. `Opt` effects are returned to the caller as `DeferredEffects`, which
//!      the router runs on the blocking pool concurrent with the socket write,
//!      off the response's critical path — so they add no latency and never
//!      block the client.
//!
//! The first `Must` failure is classified into an `Effect.Err` and handed to
//! that effect's boxed `err_handler` (invoked through the erased-callable ABI,
//! shape per `roc_call::call_handler`); its `{ status, headers, body }` reply
//! replaces the handler's own response. An empty effects list is a no-op, so
//! the escape-hatch (`effects: []`) path is unchanged.

use std::mem::ManuallyDrop;

use rusqlite::TransactionBehavior;

use crate::abi;
use crate::effects::{checkout_sqlite, send_request, sql_param_to_rusqlite};
use crate::roc_call::{copy_headers, roc_str, ResponseParts};
use crate::roc_host;
use crate::types::{
    RocDbWrite, RocEffectActionTag, RocEffectErr, RocEffectErrPayload, RocEffectErrTag,
    RocEffectTag, RocHttpSend, RocReply, RocResponse,
};

/// The host-side mirror of `Effect.Err`: the classified failure a `Must`
/// effect's `err_handler` receives.
#[derive(Debug)]
enum EffectErr {
    UniqueViolation(String),
    ForeignKeyViolation(String),
    Busy,
    HttpFailed(String),
    Other(String),
}

/// A decoded effect tag payload, copied out of the Roc `effects` list. The
/// backing Roc allocations stay owned by the response (freed by its recursive
/// decref), so these owned copies never touch refcounts.
enum Action {
    Db {
        db: String,
        sql: String,
        params: Vec<rusqlite::types::Value>,
    },
    Http {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout_ms: u64,
    },
    Log(String),
}

/// A `Must` effect: an action plus the boxed `err_handler` to invoke on failure.
struct MustItem {
    action: Action,
    err_handler: abi::RocErasedCallable,
}

/// The fire-and-forget `Opt` effects a handler returned, deferred so the caller
/// runs them AFTER the response is flushed (B3). They carry only owned data
/// (each `Action` is copied out of the Roc `effects` list, no refcounts), so
/// they safely outlive the response's decref and can move to another thread.
pub struct DeferredEffects {
    opts: Vec<Action>,
}

impl DeferredEffects {
    fn new(opts: Vec<Action>) -> Self {
        Self { opts }
    }

    /// True when there is nothing to run — the common no-effects path, letting
    /// the router skip the detached spawn entirely.
    pub fn is_empty(&self) -> bool {
        self.opts.is_empty()
    }

    /// Run every deferred `Opt` serially, best-effort (failures logged only).
    /// The router calls this on the blocking pool, detached and concurrent with
    /// hyper writing the response body, so `Opt` effects add no latency to the
    /// client. In-flight `Opt`s are lost on process shutdown, which is
    /// acceptable for fire-and-forget semantics.
    pub fn run(self) {
        run_opts(&self.opts);
    }
}

fn string_of(text: &abi::RocStr) -> String {
    String::from_utf8_lossy(text.as_slice()).into_owned()
}

/// Run the effects a handler returned; return the response to send. Reads the
/// effect data (including the boxed `err_handler` pointers) before the caller
/// decrefs `response`.
pub fn run_effects(
    response: &RocResponse,
    handler_response: ResponseParts,
) -> (ResponseParts, DeferredEffects) {
    let effects = response.effects.as_slice();
    if effects.is_empty() {
        return (handler_response, DeferredEffects::new(Vec::new()));
    }

    // Partition preserving relative order: DB `Must` first, external `Must`
    // (HttpSend/Log) after commit, `Opt` last.
    let mut db_musts: Vec<MustItem> = Vec::new();
    let mut ext_musts: Vec<MustItem> = Vec::new();
    let mut opts: Vec<Action> = Vec::new();

    for effect in effects {
        match effect.tag {
            RocEffectTag::Must => {
                let payload = effect.payload_must();
                let item = MustItem {
                    action: read_action(&payload._0),
                    err_handler: payload._1,
                };
                if matches!(item.action, Action::Db { .. }) {
                    db_musts.push(item);
                } else {
                    ext_musts.push(item);
                }
            }
            RocEffectTag::Opt => opts.push(read_action(&effect.payload_opt())),
        }
    }

    // Phase 1: DB `Must` writes in one transaction. On failure the classified
    // error goes to the failing effect's `err_handler`, whose reply becomes the
    // response; the deferred `Opt`s still fire (fire-and-forget).
    if let Some((err_handler, err)) = run_db_transaction(&db_musts) {
        let reply = invoke_err_handler(err_handler, &err);
        return (reply, DeferredEffects::new(opts));
    }

    // Phase 2: external `Must` writes after commit.
    for item in &ext_musts {
        if let Err(err) = run_action(&item.action) {
            tracing::warn!("effect: external Must failed: {err:?}");
            let reply = invoke_err_handler(item.err_handler, &err);
            return (reply, DeferredEffects::new(opts));
        }
    }

    // Phase 3: `Opt` effects are handed back for the router to run AFTER this
    // response is flushed (B3), concurrent with the socket write, so they never
    // add latency to the client. They carry only owned data, so they outlive
    // the response's decref below.
    (handler_response, DeferredEffects::new(opts))
}

/// Decode one `DbWrite | HttpSend | Log` tag payload into an owned `Action`.
fn read_action(action: &abi::DbWriteOrHttpSendOrLog) -> Action {
    match action.tag {
        RocEffectActionTag::DbWrite => {
            let write: RocDbWrite = action.payload_db_write();
            Action::Db {
                db: string_of(&write.db),
                sql: string_of(&write.sql),
                params: write.params.as_slice().iter().map(sql_param_to_rusqlite).collect(),
            }
        }
        RocEffectActionTag::HttpSend => {
            let send: RocHttpSend = action.payload_http_send();
            Action::Http {
                method: string_of(&send.method),
                url: string_of(&send.url),
                headers: copy_headers(send.headers.as_slice()),
                body: send.body.as_slice().to_vec(),
                timeout_ms: send.timeout_ms,
            }
        }
        RocEffectActionTag::Log => Action::Log(string_of(&action.payload_log())),
    }
}

/// Run all DB `Must` writes in one `BEGIN IMMEDIATE` transaction, in order.
/// Returns `Some((err_handler, err))` for the first failing write (the whole
/// transaction having rolled back), or `None` if every write committed.
fn run_db_transaction(items: &[MustItem]) -> Option<(abi::RocErasedCallable, EffectErr)> {
    let first = items.first()?;
    // Single-database-per-request assumption (see plan): the whole DB `Must`
    // group runs against the first write's database file.
    let db = match &first.action {
        Action::Db { db, .. } => db.clone(),
        _ => unreachable!("only DB Must effects reach run_db_transaction"),
    };

    let mut connection = match checkout_sqlite(&db) {
        Ok(connection) => connection,
        Err(message) => {
            tracing::error!("effect: DB checkout failed: {message}");
            return Some((first.err_handler, EffectErr::Other(message)));
        }
    };
    let tx = match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!("effect: BEGIN IMMEDIATE failed: {err}");
            return Some((first.err_handler, classify_db_error(&err)));
        }
    };

    for item in items {
        let (sql, params) = match &item.action {
            Action::Db { sql, params, .. } => (sql, params),
            _ => unreachable!("only DB Must effects reach run_db_transaction"),
        };
        let outcome = tx
            .prepare_cached(sql)
            .and_then(|mut statement| statement.execute(rusqlite::params_from_iter(params.iter())));
        if let Err(err) = outcome {
            let classified = classify_db_error(&err);
            tracing::warn!("effect: DbWrite failed ({classified:?}); rolling back transaction");
            // Dropping `tx` without commit rolls the whole group back.
            return Some((item.err_handler, classified));
        }
    }

    if let Err(err) = tx.commit() {
        tracing::error!("effect: COMMIT failed: {err}");
        let last = items.last().expect("items is non-empty");
        return Some((last.err_handler, classify_db_error(&err)));
    }
    None
}

/// Run one external `Must` (or `Opt`) action to completion.
fn run_action(action: &Action) -> Result<(), EffectErr> {
    match action {
        Action::Http {
            method,
            url,
            headers,
            body,
            timeout_ms,
        } => send_request(
            method.clone(),
            url.clone(),
            headers.clone(),
            body.clone(),
            *timeout_ms,
        )
        .map(|_| ())
        .map_err(EffectErr::HttpFailed),
        Action::Log(message) => {
            tracing::info!(target: "roc", "{message}");
            Ok(())
        }
        // A DbWrite only reaches here as an `Opt`: run it autocommit.
        Action::Db { db, sql, params } => {
            let connection = checkout_sqlite(db).map_err(EffectErr::Other)?;
            connection
                .prepare_cached(sql)
                .and_then(|mut statement| statement.execute(rusqlite::params_from_iter(params.iter())))
                .map(|_| ())
                .map_err(|err| classify_db_error(&err))
        }
    }
}

/// Run `Opt` effects best-effort; failures are logged only (B3).
fn run_opts(opts: &[Action]) {
    for action in opts {
        if let Err(err) = run_action(action) {
            tracing::warn!("effect: Opt failed (ignored): {err:?}");
        }
    }
}

/// Map a `rusqlite` error to the typed `Effect.Err` variant.
fn classify_db_error(err: &rusqlite::Error) -> EffectErr {
    if let rusqlite::Error::SqliteFailure(inner, message) = err {
        let detail = message.clone().unwrap_or_else(|| err.to_string());
        match inner.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
                return EffectErr::UniqueViolation(detail);
            }
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                return EffectErr::ForeignKeyViolation(detail);
            }
            _ => {}
        }
        if matches!(
            inner.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ) {
            return EffectErr::Busy;
        }
        return EffectErr::Other(detail);
    }
    EffectErr::Other(err.to_string())
}

/// Invoke a `Must` effect's boxed `err_handler` with a classified error and
/// copy out its `{ status, headers, body }` reply. Same invoke shape as
/// `call_handler`: read the erased callable's payload header for the fn ptr
/// and its capture slot for the environment, then call through the uniform
/// ABI. The args buffer (whose `Str` payload we build here) is consumed by the
/// callee; the returned reply is owned by us and released after copying out.
/// The callable itself is freed later by the response's recursive decref.
fn invoke_err_handler(callable: abi::RocErasedCallable, err: &EffectErr) -> ResponseParts {
    #[repr(C)]
    struct ErrArgs {
        arg0: RocEffectErr,
    }

    let host = roc_host();
    let args = ErrArgs {
        arg0: build_err_arg(err),
    };

    let mut ret = core::mem::MaybeUninit::<RocReply>::uninit();
    unsafe {
        let header = abi::roc_erased_callable_payload_ptr(callable);
        let capture = abi::roc_erased_callable_capture_ptr(callable);
        ((*header).callable_fn_ptr)(
            host as *const abi::RocHost as *mut abi::RocHost,
            ret.as_mut_ptr() as *mut u8,
            &args as *const ErrArgs as *const u8,
            capture,
        );
        let reply = ret.assume_init();
        let out = ResponseParts {
            status: reply.status,
            headers: copy_headers(reply.headers.as_slice()),
            body: reply.body.as_slice().to_vec(),
        };
        reply.decref(host);
        out
    }
}

/// Build the Roc `Effect.Err` argument for an `err_handler` invocation.
fn build_err_arg(err: &EffectErr) -> RocEffectErr {
    match err {
        EffectErr::UniqueViolation(detail) => RocEffectErr {
            tag: RocEffectErrTag::UniqueViolation,
            payload: RocEffectErrPayload {
                unique_violation: ManuallyDrop::new(roc_str(detail)),
            },
        },
        EffectErr::ForeignKeyViolation(detail) => RocEffectErr {
            tag: RocEffectErrTag::ForeignKeyViolation,
            payload: RocEffectErrPayload {
                foreign_key_violation: ManuallyDrop::new(roc_str(detail)),
            },
        },
        EffectErr::HttpFailed(detail) => RocEffectErr {
            tag: RocEffectErrTag::HttpFailed,
            payload: RocEffectErrPayload {
                http_failed: ManuallyDrop::new(roc_str(detail)),
            },
        },
        EffectErr::Other(detail) => RocEffectErr {
            tag: RocEffectErrTag::Other,
            payload: RocEffectErrPayload {
                other: ManuallyDrop::new(roc_str(detail)),
            },
        },
        EffectErr::Busy => RocEffectErr {
            tag: RocEffectErrTag::Busy,
            payload: RocEffectErrPayload { busy: [] },
        },
    }
}
