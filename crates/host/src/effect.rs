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

use crate::abi;
use crate::db::SqlValue;
use crate::effects::send_request;
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
        params: Vec<SqlValue>,
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
                params: write.params.as_slice().iter().map(crate::db::param_from_host).collect(),
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
    let writes: Vec<crate::db::Write> = items
        .iter()
        .map(|item| match &item.action {
            Action::Db { sql, params, .. } => (sql.clone(), params.clone()),
            _ => unreachable!("only DB Must effects reach run_db_transaction"),
        })
        .collect();

    match crate::cursor::run_write_txn(&db, &writes) {
        Ok(()) => {
            purge_written_tables(writes.iter().map(|(sql, _)| sql.as_str()));
            None
        }
        Err((index, err)) => {
            let classified = EffectErr::from_db(err);
            tracing::warn!("effect: DbWrite failed ({classified:?}); rolled back transaction");
            Some((items[index].err_handler, classified))
        }
    }
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
            crate::cursor::run_autocommit(db, sql, params).map_err(EffectErr::from_db)?;
            purge_written_tables(std::iter::once(sql.as_str()));
            Ok(())
        }
    }
}

/// After writes commit, purge every response-cache entry that read a mutated
/// table (no-op unless the app enabled the cache). This is the platform's
/// invalidation hook: every mutation is a declarative `DbWrite` through this one
/// executor, so the cache stays coherent without handlers knowing it exists.
fn purge_written_tables<'a>(sqls: impl Iterator<Item = &'a str>) {
    if crate::cache::global().is_none() {
        return;
    }
    let mut seen: Vec<&str> = Vec::new();
    for sql in sqls {
        if let Some(table) = write_target_table(sql)
            && !seen.contains(&table)
        {
            seen.push(table);
            crate::cache::purge_table_global(table);
        }
    }
}

/// Best-effort extraction of the table a write mutates: `INSERT [OR ..] INTO t`,
/// `REPLACE INTO t`, `UPDATE [OR ..] t`, `DELETE FROM t`. An unrecognized shape
/// yields `None` — the TTL still bounds staleness in that case.
fn write_target_table(sql: &str) -> Option<&str> {
    let mut tokens = sql.split_whitespace();
    match tokens.next()?.to_ascii_uppercase().as_str() {
        "INSERT" | "REPLACE" => {
            // Skip any `OR IGNORE`/`OR REPLACE` up to `INTO`.
            tokens.by_ref().find(|t| t.eq_ignore_ascii_case("INTO"))?;
            tokens.next().map(strip_table_ident)
        }
        "UPDATE" => {
            let next = tokens.next()?;
            if next.eq_ignore_ascii_case("OR") {
                tokens.next(); // conflict action
                tokens.next().map(strip_table_ident)
            } else {
                Some(strip_table_ident(next))
            }
        }
        "DELETE" => {
            tokens.by_ref().find(|t| t.eq_ignore_ascii_case("FROM"))?;
            tokens.next().map(strip_table_ident)
        }
        _ => None,
    }
}

/// Trim quoting and cut a table token at the first non-identifier char (so
/// `articles(id,...)` or `"users"` resolve to the bare name).
fn strip_table_ident(tok: &str) -> &str {
    let tok = tok.trim_matches(|c| matches!(c, '"' | '`' | '\'' | '[' | ']'));
    let end = tok
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(tok.len());
    &tok[..end]
}

/// Run `Opt` effects best-effort; failures are logged only (B3).
fn run_opts(opts: &[Action]) {
    for action in opts {
        if let Err(err) = run_action(action) {
            tracing::warn!("effect: Opt failed (ignored): {err:?}");
        }
    }
}

impl EffectErr {
    /// Lift a backend's engine-neutral `DbError` into the effect error.
    fn from_db(err: crate::db::DbError) -> Self {
        match err {
            crate::db::DbError::UniqueViolation(d) => EffectErr::UniqueViolation(d),
            crate::db::DbError::ForeignKeyViolation(d) => EffectErr::ForeignKeyViolation(d),
            crate::db::DbError::Busy => EffectErr::Busy,
            crate::db::DbError::Other(d) => EffectErr::Other(d),
        }
    }
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
        // Error-handler replies are small and off the hot path — copy the body
        // and release the reply immediately rather than pinning the arena.
        let out = ResponseParts {
            status: reply.status,
            headers: copy_headers(reply.headers.as_slice()),
            body: crate::body::ResponseBody::Copied(reply.body.as_slice().to_vec()),
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

#[cfg(test)]
mod tests {
    use super::write_target_table;

    #[test]
    fn extracts_the_mutated_table_from_each_write_shape() {
        let cases = [
            ("INSERT INTO articles (id, slug) VALUES (?, ?)", Some("articles")),
            ("INSERT OR IGNORE INTO article_tags (article_id, tag) VALUES (?, ?)", Some("article_tags")),
            ("REPLACE INTO favorites VALUES (?, ?)", Some("favorites")),
            ("UPDATE articles SET slug = ? WHERE id = ?", Some("articles")),
            ("UPDATE OR IGNORE follows SET x = 1", Some("follows")),
            ("DELETE FROM comments WHERE article_id = ?", Some("comments")),
            ("DELETE FROM favorites WHERE user_id = ? AND article_id = ?", Some("favorites")),
            ("SELECT * FROM articles", None),
        ];
        for (sql, want) in cases {
            assert_eq!(write_target_table(sql), want, "sql: {sql}");
        }
    }

    #[test]
    fn strips_quoting_and_column_paren() {
        assert_eq!(write_target_table("INSERT INTO \"users\" VALUES (1)"), Some("users"));
        assert_eq!(write_target_table("INSERT INTO articles(id) VALUES (1)"), Some("articles"));
    }
}
