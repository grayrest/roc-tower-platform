//! Env-gated request profiler (`TP_PROFILE=1`): splits per-request service time
//! into DB execution, cell marshalling, and Roc compute/serialize. Zero cost
//! when disabled (one cached bool load). Drive the server single-connection
//! (`wrk -c1 -t1`) so there is no queueing and wall time ≈ CPU time.
//!
//! To avoid per-cell clock inflation, only the coarse regions are timed with
//! `Instant` (SQL begin/step/finish = DB execution; the whole handler); the
//! per-cell getters are merely *counted* (a thread-local increment, no clock).
//! The Roc-side bucket (`handler - db_exec`) therefore holds marshalling +
//! record building + `Json.to_str`; the getter count bounds the marshalling
//! share (≈ getters × ~70ns, the B4 per-cell copy cost).

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

#[inline]
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("TP_PROFILE").is_ok())
}

thread_local! {
    static EXEC_NS: Cell<u64> = const { Cell::new(0) };
    static GETTERS: Cell<u64> = const { Cell::new(0) };
}

/// RAII timer for a DB-execution region (SQL begin/step/finish). `None` when
/// profiling is off, so the timer never runs.
pub struct ExecTimer(Instant);

#[inline]
pub fn exec() -> Option<ExecTimer> {
    enabled().then(|| ExecTimer(Instant::now()))
}

impl Drop for ExecTimer {
    fn drop(&mut self) {
        let ns = self.0.elapsed().as_nanos() as u64;
        EXEC_NS.with(|c| c.set(c.get() + ns));
    }
}

/// Count one cell-getter FFI call (no clock read — bounds the marshalling share
/// without inflating it).
#[inline]
pub fn getter() {
    if enabled() {
        GETTERS.with(|c| c.set(c.get() + 1));
    }
}

static N: AtomicU64 = AtomicU64::new(0);
static SUM_HANDLER: AtomicU64 = AtomicU64::new(0);
static SUM_EXEC: AtomicU64 = AtomicU64::new(0);
static SUM_GETTERS: AtomicU64 = AtomicU64::new(0);
static SUM_DISPATCH: AtomicU64 = AtomicU64::new(0);

const WINDOW: u64 = 20_000;

/// Record one handler's timing (called on the blocking thread that ran the
/// handler, so the thread-local DB timers are the ones this handler touched).
pub fn record_handler(handler_ns: u64) {
    if !enabled() {
        return;
    }
    let exec = EXEC_NS.with(|c| c.replace(0));
    let getters = GETTERS.with(|c| c.replace(0));
    let n = N.fetch_add(1, Relaxed) + 1;
    SUM_HANDLER.fetch_add(handler_ns, Relaxed);
    SUM_EXEC.fetch_add(exec, Relaxed);
    SUM_GETTERS.fetch_add(getters, Relaxed);
    if n.is_multiple_of(WINDOW) {
        let h = SUM_HANDLER.load(Relaxed) / n;
        let e = SUM_EXEC.load(Relaxed) / n;
        let g = SUM_GETTERS.load(Relaxed) / n;
        let d = SUM_DISPATCH.load(Relaxed) / n;
        let roc = h.saturating_sub(e);
        let host = d.saturating_sub(h);
        tracing::info!(
            "[profile] n={n} req={d}ns = host {host}ns + handler {h}ns [ db_exec {e}ns + roc_side {roc}ns ({g} cell getters ~= {}ns marshal) ]",
            g * 70
        );
    }
}

/// Record the whole-dispatch (server-side, ex-HTTP-parse) time. Recorded from
/// the async task; averaged separately, so cross-thread is fine.
pub fn record_dispatch(dispatch_ns: u64) {
    if enabled() {
        SUM_DISPATCH.fetch_add(dispatch_ns, Relaxed);
    }
}

/// Marker to time a region and return elapsed ns; `None` when off.
#[inline]
pub fn now() -> Option<Instant> {
    enabled().then(Instant::now)
}
