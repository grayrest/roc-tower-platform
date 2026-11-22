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


use crate::abi;
use crate::db::SqlValue;
use crate::effects::send_request;
use crate::roc_call::{copy_headers, roc_str, ResponseParts};
use crate::types::{
    RocEffect, RocEffectAction, RocEffectActionView, RocEffectErr, RocEffectView, RocErrHandler,
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
/// backing Roc allocations stay owned by the list, so these owned copies never
/// touch refcounts.
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
    err_handler: RocErrHandler,
}

/// The fire-and-forget `Opt` effects a handler returned, deferred so the caller
/// runs them AFTER the response is flushed (B3). They carry only owned data
/// (each `Action` is copied out of the Roc `effects` list, no refcounts), so
/// they safely outlive the list and can move to another thread.
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

/// Run the effects a handler returned; return the response to send. Each
/// `Must` keeps its own reference to its `err_handler`.
pub fn run_effects(
    effects: &abi::RocList<RocEffect>,
    handler_response: ResponseParts,
) -> (ResponseParts, DeferredEffects) {
    let effects = effects.as_slice();
    if effects.is_empty() {
        return (handler_response, DeferredEffects::new(Vec::new()));
    }

    // Partition preserving relative order: DB `Must` first, external `Must`
    // (HttpSend/Log) after commit, `Opt` last.
    let mut db_musts: Vec<MustItem> = Vec::new();
    let mut ext_musts: Vec<MustItem> = Vec::new();
    let mut opts: Vec<Action> = Vec::new();

    for effect in effects {
        match effect.view() {
            RocEffectView::Must(payload) => {
                let item = MustItem {
                    action: read_action(&payload._0),
                    err_handler: payload._1.clone(),
                };
                // D12: a Must DbWrite/HttpSend is incompatible with a cache
                // hit that skips the handler ("must run" would silently stop
                // firing), and a Must DbWrite would store this response's own
                // pre-write read — poison. Must Log stays neutral; Opt
                // effects never poison (best-effort contract; their writes
                // are covered by the write-set purge below).
                if matches!(item.action, Action::Db { .. } | Action::Http { .. }) {
                    crate::effects::poison_current_ctx();
                }
                if matches!(item.action, Action::Db { .. }) {
                    db_musts.push(item);
                } else {
                    ext_musts.push(item);
                }
            }
            RocEffectView::Opt(action) => opts.push(read_action(action)),
        }
    }

    // Phase 1: DB `Must` writes in one transaction. On failure the classified
    // error goes to the failing effect's `err_handler`, whose reply becomes the
    // response; the deferred `Opt`s still fire (fire-and-forget).
    if let Some((err_handler, err)) = run_db_transaction(&db_musts) {
        let reply = invoke_err_handler(&err_handler, &err);
        return (reply, DeferredEffects::new(opts));
    }

    // Phase 2: external `Must` writes after commit.
    for item in &ext_musts {
        if let Err(err) = run_action(&item.action) {
            tracing::warn!("effect: external Must failed: {err:?}");
            let reply = invoke_err_handler(&item.err_handler, &err);
            return (reply, DeferredEffects::new(opts));
        }
    }

    // Phase 3: `Opt` effects are handed back for the router to run AFTER this
    // response is flushed (B3), concurrent with the socket write, so they never
    // add latency to the client. They carry only owned data, so they outlive
    // the effects list.
    (handler_response, DeferredEffects::new(opts))
}

/// Decode one `DbWrite | HttpSend | Log` tag payload into an owned `Action`.
fn read_action(action: &RocEffectAction) -> Action {
    match action.view() {
        RocEffectActionView::DbWrite { db, params, sql } => Action::Db {
            db: string_of(db),
            sql: string_of(sql),
            params: params.as_slice().iter().map(crate::db::param_from_host).collect(),
        },
        RocEffectActionView::HttpSend { timeout_ms, body, headers, method, url } => Action::Http {
            method: string_of(method),
            url: string_of(url),
            headers: copy_headers(headers.as_slice()),
            body: body.as_slice().to_vec(),
            timeout_ms: *timeout_ms,
        },
        RocEffectActionView::Log(text) => Action::Log(string_of(text)),
    }
}

/// Run all DB `Must` writes in one `BEGIN IMMEDIATE` transaction, in order.
/// Returns `Some((err_handler, err))` for the first failing write (the whole
/// transaction having rolled back), or `None` if every write committed.
fn run_db_transaction(items: &[MustItem]) -> Option<(RocErrHandler, EffectErr)> {
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
            purge_written_tables(&db, writes.iter().map(|(sql, _)| sql.as_str()));
            None
        }
        Err((index, err)) => {
            let classified = EffectErr::from_db(err);
            tracing::warn!("effect: DbWrite failed ({classified:?}); rolled back transaction");
            Some((items[index].err_handler.clone(), classified))
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
            purge_written_tables(db, std::iter::once(sql.as_str()));
            Ok(())
        }
    }
}

/// After writes commit, purge every response-cache entry whose read columns
/// intersect the mutated ones (no-op unless the app enabled the cache). This
/// is the platform's invalidation hook: every mutation is a declarative
/// `DbWrite` through this one executor, so the cache stays coherent without
/// handlers knowing it exists.
///
/// The write set comes from the analysis shadow — authoritative and
/// TRIGGER-EXPANDED (preparing `INSERT INTO favorites …` reports the
/// trigger-carried `articles.favorites_count` update), column-scoped where
/// SQLite reports columns (UPDATE), whole-table where it doesn't
/// (INSERT/DELETE). Runtime DDL flushes everything: `analyze` already
/// mirrored it into the shadow and dropped the memoized derivations; the
/// epoch bump covers in-flight readers (R4). Anything underivable fails
/// CLOSED: the legacy single-table parse, else a full flush — never silent
/// under-invalidation (D9).
fn purge_written_tables<'a>(db: &str, sqls: impl Iterator<Item = &'a str>) {
    if crate::cache::global().is_none() {
        return;
    }
    for sql in sqls {
        match crate::shadow::analyze(db, sql) {
            Ok(analysis) if analysis.schema_change => crate::cache::purge_all_global(),
            Ok(analysis) if analysis.opaque => purge_fallback(sql),
            Ok(analysis) => {
                for (table, cols) in &analysis.writes {
                    crate::cache::purge_write_global(table, cols);
                }
            }
            Err(err) => {
                tracing::warn!(%err, sql, "write purge: underivable; falling back");
                purge_fallback(sql);
            }
        }
    }
}

/// Conservative purge for a write the shadow couldn't attribute: the legacy
/// single-table parse at full-column width, else flush the whole cache.
fn purge_fallback(sql: &str) {
    match write_target_table(sql) {
        Some(table) => crate::cache::purge_table_global(table),
        None => crate::cache::purge_all_global(),
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
/// copy out its `{ status, headers, body }` reply. The error argument moves
/// into the call; the reply is ours and is released as it drops.
fn invoke_err_handler(err_handler: &RocErrHandler, err: &EffectErr) -> ResponseParts {
    let reply = err_handler.call(build_err_arg(err));
    // Error-handler replies are small and off the hot path — copy the body and
    // release the reply immediately rather than pinning the arena.
    ResponseParts {
        status: reply.status,
        headers: copy_headers(reply.headers.as_slice()),
        body: crate::body::ResponseBody::Copied(reply.body.as_slice().to_vec()),
    }
}

/// Build the Roc `Effect.Err` argument for an `err_handler` invocation.
fn build_err_arg(err: &EffectErr) -> RocEffectErr {
    match err {
        EffectErr::UniqueViolation(detail) => RocEffectErr::unique_violation(roc_str(detail)),
        EffectErr::ForeignKeyViolation(detail) => RocEffectErr::foreign_key_violation(roc_str(detail)),
        EffectErr::HttpFailed(detail) => RocEffectErr::http_failed(roc_str(detail)),
        EffectErr::Other(detail) => RocEffectErr::other(roc_str(detail)),
        EffectErr::Busy => RocEffectErr::busy(),
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
