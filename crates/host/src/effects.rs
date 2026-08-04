//! Hosted function implementations — the effects available to Roc code.
//!
//! Ownership contract (see the generated glue header): hosted functions own
//! their refcounted arguments and must release them; returned values transfer
//! ownership to Roc.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(not(feature = "turso"))]
use r2d2_sqlite::SqliteConnectionManager;

use crate::abi;
use crate::roc_call::{roc_kv_list, roc_str};
use crate::roc_host;
use crate::types::RocHttpOk;

/// Handle to the server's tokio runtime, used to drive async effects
/// (outbound HTTP) from the blocking threads Roc handlers run on.
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Upper bound for `Host.random_bytes!` to keep a buggy app from allocating
/// unbounded memory in one call.
const MAX_RANDOM_BYTES: u64 = 1024 * 1024;

pub fn set_runtime(handle: tokio::runtime::Handle) {
    let _ = RUNTIME.set(handle);
}

fn runtime() -> &'static tokio::runtime::Handle {
    RUNTIME.get().expect("effects::set_runtime must be called before Roc code runs")
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client")
    })
}

/// Release a refcounted-element Roc list: tear down elements when this is the
/// last reference, then release the list allocation itself.
pub(crate) unsafe fn decref_list_with<T: Copy>(list: abi::RocList<T>, drop_item: impl Fn(T)) {
    if list.has_one_ref() {
        for item in list.allocation_items() {
            drop_item(*item);
        }
    }
    unsafe { list.decref(roc_host()) };
}

/// Per-file connection pool (spike I3: "pool-for-everything"). Cursor reads
/// (`roc_sql_begin`, which pins a connection for the cursor's life) and the
/// effect executor's `BEGIN IMMEDIATE` write transactions both check out from
/// the same pool, so
/// the process runs a single DB topology with explicit pragmas instead of the
/// old shared-`Mutex<Connection>` cache (which corrupted concurrent
/// transactions).
#[cfg(not(feature = "turso"))]
type SqlitePool = r2d2::Pool<SqliteConnectionManager>;
#[cfg(not(feature = "turso"))]
pub(crate) type PooledSqlite = r2d2::PooledConnection<SqliteConnectionManager>;

#[cfg(not(feature = "turso"))]
const SQLITE_POOL_MAX: u32 = 16;
#[cfg(not(feature = "turso"))]
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(not(feature = "turso"))]
fn sqlite_pools() -> &'static Mutex<HashMap<String, SqlitePool>> {
    static POOLS: OnceLock<Mutex<HashMap<String, SqlitePool>>> = OnceLock::new();
    POOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Simple effects.
// ---------------------------------------------------------------------------

/// `Host.log! : Str => {}`
#[unsafe(no_mangle)]
pub extern "C" fn roc_log(arg0: abi::RocStr) {
    tracing::info!(target: "roc", "{}", String::from_utf8_lossy(arg0.as_slice()));
    unsafe { arg0.decref(roc_host()) };
}

/// `Host.env_var! : Str => Try(Str, [EnvVarNotFound])`
#[unsafe(no_mangle)]
pub extern "C" fn roc_env_var(arg0: abi::RocStr) -> abi::HostEnvVarResult {
    let name = String::from_utf8_lossy(arg0.as_slice()).into_owned();
    unsafe { arg0.decref(roc_host()) };
    match std::env::var(&name) {
        Ok(value) => abi::HostEnvVarResult {
            tag: abi::HostEnvVarResultTag::Ok,
            payload: abi::HostEnvVarResultPayload {
                ok: ManuallyDrop::new(roc_str(&value)),
            },
        },
        Err(_) => abi::HostEnvVarResult {
            tag: abi::HostEnvVarResultTag::Err,
            payload: abi::HostEnvVarResultPayload { err: [] },
        },
    }
}

/// `Host.next_id! : Str => I64` — reserve the next persistent monotonic id for
/// the named database (backs `Req.reserved_id!`). A reservation failure is a
/// catastrophic DB error; panic (`C-unwind`, caught by the router's
/// `catch_unwind` → 500) rather than hand back a colliding id.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn roc_next_id(db: abi::RocStr) -> i64 {
    poison_current_ctx();
    let db = db.as_str();
    crate::cursor::reserve_next_id(db)
        .unwrap_or_else(|err| panic!("next_id!({db:?}): {err}"))
}

/// `Host.now_ms! : () => U64`
#[unsafe(no_mangle)]
pub extern "C" fn roc_now_ms() -> u64 {
    poison_current_ctx();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// `Host.random_bytes! : U64 => List(U8)`
#[unsafe(no_mangle)]
pub extern "C" fn roc_random_bytes(count: u64) -> abi::RocListWith<u8, false> {
    poison_current_ctx();
    let count = count.min(MAX_RANDOM_BYTES) as usize;
    let mut buffer = vec![0u8; count];
    if let Err(err) = getrandom::fill(&mut buffer) {
        // A failing OS entropy source is not something the app can handle.
        panic!("random_bytes: OS entropy source failed: {err}");
    }
    unsafe { abi::RocListWith::<u8, false>::from_slice(&buffer, roc_host()) }
}

/// `Host.password_hash! : Str => Str` — argon2id with a fresh OS-random salt,
/// returned as a PHC string. Hashing with default params cannot fail on valid
/// input; if it somehow does, log and return an empty string, which
/// `password_verify` can never match.
#[unsafe(no_mangle)]
pub extern "C" fn password_hash(arg0: abi::RocStr) -> abi::RocStr {
    poison_current_ctx();
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};

    let password = arg0.as_slice().to_vec();
    unsafe { arg0.decref(roc_host()) };
    let salt = SaltString::generate(&mut OsRng);
    match argon2::Argon2::default().hash_password(&password, &salt) {
        Ok(hash) => roc_str(&hash.to_string()),
        Err(err) => {
            tracing::error!("password_hash: argon2 failed: {err}");
            roc_str("")
        }
    }
}

/// `Host.password_verify! : Str, Str => Bool` — verify a password against a
/// stored PHC string; false on mismatch or an unparseable hash.
#[unsafe(no_mangle)]
pub extern "C" fn password_verify(arg0: abi::RocStr, arg1: abi::RocStr) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};

    let password = arg0.as_slice().to_vec();
    let stored = String::from_utf8_lossy(arg1.as_slice()).into_owned();
    unsafe {
        arg0.decref(roc_host());
        arg1.decref(roc_host());
    }
    match PasswordHash::new(&stored) {
        Ok(parsed) => argon2::Argon2::default()
            .verify_password(&password, &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Outbound HTTP.
// ---------------------------------------------------------------------------

/// `Host.http_send!` — send an HTTP request, driven on the server's runtime.
#[unsafe(no_mangle)]
pub extern "C" fn roc_http_send(args: abi::HostHttpSendArgs) -> abi::HostHttpSendResult {
    poison_current_ctx();
    let host = roc_host();
    let method = String::from_utf8_lossy(args.method.as_slice()).into_owned();
    let url = String::from_utf8_lossy(args.url.as_slice()).into_owned();
    let headers: Vec<(String, String)> = args
        .headers
        .as_slice()
        .iter()
        .map(|kv| {
            (
                String::from_utf8_lossy(kv.name.as_slice()).into_owned(),
                String::from_utf8_lossy(kv.value.as_slice()).into_owned(),
            )
        })
        .collect();
    let body = args.body.as_slice().to_vec();
    let timeout_ms = args.timeout_ms;
    unsafe {
        args.body.decref(host);
        decref_list_with(args.headers, |kv| kv.decref(host));
        args.method.decref(host);
        args.url.decref(host);
    }

    match send_request(method, url, headers, body, timeout_ms) {
        Ok((status, response_headers, response_body)) => abi::HostHttpSendResult {
            tag: abi::HostHttpSendResultTag::Ok,
            payload: abi::HostHttpSendResultPayload {
                ok: ManuallyDrop::new(RocHttpOk {
                    body: unsafe {
                        abi::RocListWith::<u8, false>::from_slice(&response_body, host)
                    },
                    headers: roc_kv_list(&response_headers),
                    status,
                }),
            },
        },
        Err(message) => abi::HostHttpSendResult {
            tag: abi::HostHttpSendResultTag::Err,
            payload: abi::HostHttpSendResultPayload {
                err: ManuallyDrop::new(roc_str(&message)),
            },
        },
    }
}

type HttpResponseParts = (u16, Vec<(String, String)>, Vec<u8>);

pub(crate) fn send_request(
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout_ms: u64,
) -> Result<HttpResponseParts, String> {
    let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
        .map_err(|_| format!("invalid HTTP method {method:?}"))?;
    let mut request = http_client().request(method, &url);
    if timeout_ms > 0 {
        request = request.timeout(Duration::from_millis(timeout_ms));
    }
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body);
    }

    runtime().block_on(async move {
        let response = request.send().await.map_err(|err| err.to_string())?;
        let status = response.status().as_u16();
        let response_headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let bytes = response.bytes().await.map_err(|err| err.to_string())?;
        Ok((status, response_headers, bytes.to_vec()))
    })
}

// ---------------------------------------------------------------------------
// SQLite.
// ---------------------------------------------------------------------------

/// Convert an engine-neutral bound param to a rusqlite value at bind time.
#[cfg(not(feature = "turso"))]
pub(crate) fn to_rusqlite(value: &crate::db::SqlValue) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match value {
        crate::db::SqlValue::Null => Value::Null,
        crate::db::SqlValue::Int(n) => Value::Integer(*n),
        crate::db::SqlValue::Real(f) => Value::Real(*f),
        crate::db::SqlValue::Text(s) => Value::Text(s.clone()),
        crate::db::SqlValue::Blob(b) => Value::Blob(b.clone()),
    }
}

/// Get (or lazily build) the connection pool for `db`. Every connection is
/// initialised with `busy_timeout=5s`, WAL journalling, and
/// `synchronous=NORMAL` (spike I3's explicit-pragma topology).
#[cfg(not(feature = "turso"))]
pub(crate) fn sqlite_pool(db: &str) -> Result<SqlitePool, String> {
    let mut pools = sqlite_pools()
        .lock()
        .map_err(|_| "sqlite pool registry poisoned".to_string())?;
    if let Some(pool) = pools.get(db) {
        return Ok(pool.clone());
    }
    // Establish WAL mode once, on a lone connection, BEFORE the pool builds
    // its connections. `Pool::build` opens `min_idle` (= max_size) connections
    // concurrently, and on a fresh database each one's `journal_mode=WAL` in
    // `with_init` would race on the one-time WAL-mode switch (a brief exclusive
    // lock that `busy_timeout` does not cover for a mode transition), producing
    // a transient r2d2 "database is locked". Once WAL is the file's persistent
    // mode, every pooled connection's `journal_mode=WAL` is a no-op with no
    // switch to race on. This runs under the pool-registry lock, so it is the
    // sole accessor here.
    {
        let init = rusqlite::Connection::open(db)
            .map_err(|err| format!("cannot open database {db:?}: {err}"))?;
        init.busy_timeout(SQLITE_BUSY_TIMEOUT)
            .and_then(|()| init.pragma_update(None, "journal_mode", "WAL"))
            .map_err(|err| format!("cannot enable WAL on {db:?}: {err}"))?;
    }
    let manager = SqliteConnectionManager::file(db).with_init(|connection| {
        // Room for every distinct statement a request mix repeats (rusqlite
        // default is 16; the RealWorld example alone has ~20).
        connection.set_prepared_statement_cache_capacity(64);
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    });
    let pool = r2d2::Pool::builder()
        .max_size(SQLITE_POOL_MAX)
        .build(manager)
        .map_err(|err| format!("cannot open database {db:?}: {err}"))?;
    pools.insert(db.to_owned(), pool.clone());
    Ok(pool)
}

/// Check out one pooled connection for `db`. The caller returns it to the pool
/// by dropping it. Coeffect reads check out per statement (autocommit); the
/// effect executor checks out once for a `BEGIN IMMEDIATE` transaction.
#[cfg(not(feature = "turso"))]
pub(crate) fn checkout_sqlite(db: &str) -> Result<PooledSqlite, String> {
    sqlite_pool(db)?
        .get()
        .map_err(|err| format!("sqlite checkout for {db:?}: {err}"))
}

// ---------------------------------------------------------------------------
// Per-request coeffect memo registry.
//
// Each in-flight request owns one `RequestState` in a process-wide registry,
// keyed by a host-minted u64 ctx id that travels through Roc inside the
// request record (`req.ctx`). Hosted memo fns index the registry by that id,
// so concurrent requests get disjoint stores. A Drop guard (`CtxGuard`) held
// across the handler call on the async side of `spawn_blocking` removes the
// entry and releases every retained box on normal return AND on panic-unwind.
// (Roc `crash` is process::exit — out of scope.)
//
// Values are boxed THUNKS (`Box({} => a)` = erased callables) stored as
// `usize` (raw pointers are not Send). A raw `Box(a)` cannot be released by
// the host: freeing needs the payload's elements_refcounted flag + alignment
// (they set the allocation header size) and a recursive child decref, all of
// which the FFI erases. An erased callable has a FIXED layout plus a
// compiler-generated on_drop that releases the capture, so
// `decref_erased_callable` is sound.
//
// No type discriminant crosses the FFI, so a same-key re-registration can
// only be handled by releasing the displaced pointer, not by rejecting a
// type mismatch.
// ---------------------------------------------------------------------------

/// Non-idempotent effect counter. `reserve_id!` bumps this; `reserve_runs!`
/// reads it. Memoization proofs assert exact run counts against it.
static RESERVE_RUNS: AtomicU64 = AtomicU64::new(0);

/// Mints unique per-request ctx ids. Starts at 1 so 0 can mean "no ctx".
static NEXT_CTX: AtomicU64 = AtomicU64::new(1);

/// Gauge: boxes currently retained by all request stores. Balanced-teardown
/// evidence: must return to 0 whenever the registry is empty.
static LIVE_MEMO_BOXES: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct RequestState {
    /// key -> raw erased-callable payload pointer. The store holds exactly
    /// one reference to each thunk (the one transferred by `memo_put!`).
    memo: HashMap<String, usize>,
    /// The raw request body. NOT a request-record field: Roc extracts it via
    /// `Host.request_body!`, which is what tells the cache this route varies
    /// on the body (D14). `Bytes` clone is a refcount bump.
    body: bytes::Bytes,
    /// Parsed request cookies, in header order. The Cookie header is stripped
    /// from the Roc-visible header list; `Host.cookie!` serves reads from
    /// here and records each name (D13).
    cookies: Vec<(String, String)>,
    /// What this request contributes to the response cache.
    attribution: CacheAttribution,
}

#[derive(Default)]
struct CacheAttribution {
    /// Derived (table -> columns-read) union across the request's statements.
    deps: HashMap<Box<str>, crate::shadow::Cols>,
    /// Per-table cache generation sampled BEFORE the table's first read (the
    /// R13 read-then-store handshake; `insert` drops the entry if it moved).
    gens: HashMap<Box<str>, u64>,
    /// Global cache epoch sampled at the first recorded read.
    epoch: Option<u64>,
    /// Cookie NAMES read, in first-read order (variant dimension).
    cookies_read: Vec<String>,
    /// How many times the route extracted the body (a variant dimension when
    /// nonzero). A COUNT, not a flag: paired with `body_keys` it distinguishes
    /// "every read was a typed decode" from "some consumer saw raw bytes".
    body_reads: u32,
    /// Canonical body keys reported by typed decodes (`Host.set_body_key!`),
    /// one per decode, in call order. When every body read reported one, the
    /// cache keys the body variant on their fold instead of the raw hash.
    body_keys: Vec<u64>,
    /// Uncacheable: nondeterministic host call, Must DbWrite/HttpSend,
    /// underivable statement, or read-path DML.
    poisoned: bool,
}

thread_local! {
    /// The ctx whose handler is running on THIS blocking-pool thread, plus
    /// whether the request is store-eligible (the middleware's
    /// `CacheEligible` marker — bypassed traffic never pays attribution).
    /// Hosted fns that take no ctx argument (`now_ms!`, `random_bytes!`, …)
    /// use it to poison their request's cacheability. Explicit
    /// install/clear, NOT an RAII guard: a panic unwinding out of a hosted
    /// fn skips Rust drops in intervening frames (router.rs documents this),
    /// so the router clears it unconditionally after `catch_unwind`.
    static CURRENT_CTX: std::cell::Cell<(u64, bool)> = const { std::cell::Cell::new((0, false)) };
}

pub fn install_current_ctx(id: u64, track_cache: bool) {
    CURRENT_CTX.with(|cell| cell.set((id, track_cache)));
}

pub fn clear_current_ctx() {
    CURRENT_CTX.with(|cell| cell.set((0, false)));
}

/// Does the request on THIS thread record cache attribution? Zero locks —
/// the gate `sql_begin` checks before touching the shadow or any registry.
pub(crate) fn current_ctx_tracks() -> bool {
    CURRENT_CTX.with(|cell| cell.get().1)
}

/// Mark the current thread's request uncacheable. Zero-lock no-op unless the
/// request is store-eligible.
pub(crate) fn poison_current_ctx() {
    let (ctx, track) = CURRENT_CTX.with(|cell| cell.get());
    if track && ctx != 0 {
        poison_ctx(ctx);
    }
}

/// Mark `ctx`'s response uncacheable.
pub(crate) fn poison_ctx(ctx: u64) {
    let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
    if let Some(state) = registry.get_mut(&ctx) {
        state.attribution.poisoned = true;
    }
}

/// Union a statement's derived read set into the request's dep set (called
/// by `sql_begin` when the response cache is enabled). Fail-closed flags
/// poison. Observed WRITES — a "read" statement that mutates, escaping the
/// effect choke point — poison AND purge immediately (R2), so existing
/// entries on those tables can't go silently stale.
pub(crate) fn record_read(ctx: u64, analysis: &crate::shadow::SqlAnalysis) {
    // Sample generations/epoch BEFORE the statement's data read (this runs
    // in sql_begin, ahead of cursor construction): a write landing between
    // sample and read bumps the generation, so the insert-time check drops
    // the potentially-stale entry (R13).
    let cache = crate::cache::global();
    let epoch = cache.map(|cache| cache.epoch());
    let sampled: Vec<(Box<str>, u64)> = match cache {
        Some(cache) => analysis
            .reads
            .keys()
            .map(|table| (table.clone(), cache.generation(table)))
            .collect(),
        None => Vec::new(),
    };
    {
        let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
        let Some(state) = registry.get_mut(&ctx) else {
            return;
        };
        let attribution = &mut state.attribution;
        if analysis.opaque
            || analysis.schema_change
            || analysis.nondeterministic
            || !analysis.writes.is_empty()
        {
            attribution.poisoned = true;
        }
        if attribution.epoch.is_none() {
            attribution.epoch = epoch;
        }
        for (table, generation) in sampled {
            attribution.gens.entry(table).or_insert(generation);
        }
        for (table, cols) in &analysis.reads {
            attribution
                .deps
                .entry(table.clone())
                .and_modify(|existing| existing.union(cols))
                .or_insert_with(|| cols.clone());
        }
    }
    for (table, cols) in &analysis.writes {
        crate::cache::purge_write_global(table, cols);
    }
}

/// Move the request's cache attribution out of the registry — MUST run on
/// the handler thread right after `call_handler` returns (the reaper drops
/// the ctx entry before the router's oneshot resolves — R9). Returns `None`
/// when no response cache is installed.
pub(crate) fn take_cache_attribution(ctx: u64) -> Option<crate::cache::CacheDeps> {
    let cache = crate::cache::global()?;
    let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
    let state = registry.get_mut(&ctx)?;
    let mut attribution = std::mem::take(&mut state.attribution);
    let body_hash = (attribution.body_reads > 0)
        .then(|| crate::cache::body_hash_of(state.body.as_ref()));
    // Canonical key only when EVERY body read was a typed decode that
    // reported one — a raw consumer (Req.body!/text_body!/form_body!) saw
    // actual bytes, so only byte-equality (the raw hash) is safe then.
    let body_key = (attribution.body_reads > 0
        && attribution.body_keys.len() == attribution.body_reads as usize)
        .then(|| fold_body_keys(&attribution.body_keys));
    let cookies = attribution
        .cookies_read
        .into_iter()
        .map(|name| {
            let value = state
                .cookies
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, value)| value.clone());
            (name, value)
        })
        .collect();
    let deps = attribution
        .deps
        .into_iter()
        .map(|(table, cols)| {
            let generation = attribution.gens.remove(&table).unwrap_or(0);
            (table, cols, generation)
        })
        .collect();
    Some(crate::cache::CacheDeps {
        deps,
        epoch: attribution.epoch.unwrap_or_else(|| cache.epoch()),
        cacheable: !attribution.poisoned,
        cookies,
        body_hash,
        body_key,
    })
}

/// Fold the per-decode canonical keys into one body-variant value (FNV-style
/// chain — order-sensitive, deterministic).
fn fold_body_keys(keys: &[u64]) -> u64 {
    keys.iter().fold(0xcbf2_9ce4_8422_2325u64, |acc, key| {
        (acc ^ key).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// `Host.set_body_key! : U64, U64 => {}` — a typed body decode reports the
/// structural hash of the DECODED value. Recorded per call; the store side
/// uses the fold of these as the body-variant key when every raw read was
/// paired with one (see `take_cache_attribution`).
#[unsafe(no_mangle)]
pub extern "C" fn roc_set_body_key(ctx: u64, key: u64) {
    let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
    if let Some(state) = registry.get_mut(&ctx) {
        state.attribution.body_keys.push(key);
    }
}

/// `Host.hash_seed! : () => U64` — the process-wide random seed for canonical
/// body-key hashing (collisions not offline-craftable; the in-memory cache
/// only needs in-process stability).
#[unsafe(no_mangle)]
pub extern "C" fn roc_hash_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher as _};
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SEED.get_or_init(|| std::collections::hash_map::RandomState::new().build_hasher().finish())
}

/// Release one store-held thunk reference.
fn release_stored_thunk(raw: usize) {
    unsafe { abi::decref_erased_callable(raw as abi::RocErasedCallable, roc_host()) };
    LIVE_MEMO_BOXES.fetch_sub(1, Ordering::SeqCst);
}

fn ctx_registry() -> &'static Mutex<HashMap<u64, RequestState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, RequestState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registry entry handle for one request. Created before the handler runs;
/// its `Drop` tears the entry down and releases every retained box, which
/// runs on normal return and on Rust panic-unwind alike.
pub struct CtxGuard {
    id: u64,
    /// The request arena, held so arena-allocated memo captures stay valid
    /// through the memo teardown in `drop`. Struct fields drop *after*
    /// `Drop::drop` runs, so this reference (and, if it is the last one, the
    /// backing region) is released only once teardown has finished reading
    /// those captures. Never read directly — its whole job is that lifetime.
    #[allow(dead_code)]
    arena: Arc<crate::arena::RequestArena>,
}

impl CtxGuard {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for CtxGuard {
    fn drop(&mut self) {
        let state = {
            // Scope the lock: decref can run arbitrary payload teardown and
            // must not happen while the registry is locked.
            let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
            registry.remove(&self.id)
        };
        match state {
            Some(state) => {
                for (_key, raw) in state.memo {
                    release_stored_thunk(raw);
                }
            }
            None => tracing::error!("ctx {}: registry entry vanished before teardown", self.id),
        }
        // Backstop for cursors a handler left open on an error path (a `?`
        // between sql_begin! and sql_finish!). The router's reaper defers this
        // drop until the handler's blocking task has returned, so the sweep
        // never races a live step.
        crate::cursor::sweep_ctx_cursors(self.id);
        // `arena` drops here, after the teardown above.
    }
}

/// Mint a fresh ctx id and insert its request store, carrying the request
/// body and parsed cookies for the host-mediated coeffects. The guard takes
/// a reference to the request's arena so memo captures allocated in it
/// outlive the memo teardown.
pub fn mint_ctx(
    arena: Arc<crate::arena::RequestArena>,
    body: bytes::Bytes,
    cookies: Vec<(String, String)>,
) -> CtxGuard {
    let id = NEXT_CTX.fetch_add(1, Ordering::SeqCst);
    ctx_registry().lock().expect("ctx registry poisoned").insert(
        id,
        RequestState {
            body,
            cookies,
            ..RequestState::default()
        },
    );
    CtxGuard { id, arena }
}

/// `Host.request_body! : U64 => List(U8)` — the raw request body bytes.
/// Extraction is recorded: the route's cached responses gain a body-hash
/// variant dimension (D14). ctx 0 (init) has no body: empty.
#[unsafe(no_mangle)]
pub extern "C" fn roc_request_body(ctx: u64) -> abi::RocListWith<u8, false> {
    let body = {
        let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
        match registry.get_mut(&ctx) {
            Some(state) => {
                state.attribution.body_reads += 1;
                state.body.clone()
            }
            None => {
                if ctx != 0 {
                    tracing::error!("request_body!({ctx}): no registry entry for ctx");
                }
                bytes::Bytes::new()
            }
        }
    };
    unsafe { abi::RocListWith::<u8, false>::from_slice(&body, roc_host()) }
}

/// `Host.cookie! : U64, Str => Try(Str, [NoCookie])` — the named request
/// cookie (first match wins). Each read is recorded by NAME so cached
/// responses vary on exactly the cookies the route reads (D13).
#[unsafe(no_mangle)]
pub extern "C" fn roc_cookie(ctx: u64, name: abi::RocStr) -> abi::HostCookieResult {
    let host = roc_host();
    let wanted = String::from_utf8_lossy(name.as_slice()).into_owned();
    unsafe { name.decref(host) };

    let found = {
        let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
        match registry.get_mut(&ctx) {
            Some(state) => {
                if !state.attribution.cookies_read.contains(&wanted) {
                    state.attribution.cookies_read.push(wanted.clone());
                }
                state
                    .cookies
                    .iter()
                    .find(|(candidate, _)| *candidate == wanted)
                    .map(|(_, value)| value.clone())
            }
            None => {
                if ctx != 0 {
                    tracing::error!("cookie!({ctx}, {wanted:?}): no registry entry for ctx");
                }
                None
            }
        }
    };
    match found {
        Some(value) => abi::HostCookieResult {
            tag: abi::HostCookieResultTag::Ok,
            payload: abi::HostCookieResultPayload {
                ok: ManuallyDrop::new(roc_str(&value)),
            },
        },
        None => abi::HostCookieResult {
            tag: abi::HostCookieResultTag::Err,
            payload: abi::HostCookieResultPayload { err: [] },
        },
    }
}

/// `Host.reserve_id! : () => U64` — the non-idempotent injector effect.
#[unsafe(no_mangle)]
pub extern "C" fn roc_reserve_id() -> u64 {
    poison_current_ctx();
    RESERVE_RUNS.fetch_add(1, Ordering::SeqCst) + 1
}

/// `Host.reserve_runs! : () => U64` — how many times `reserve_id!` has run.
#[unsafe(no_mangle)]
pub extern "C" fn roc_reserve_runs() -> u64 {
    poison_current_ctx();
    RESERVE_RUNS.load(Ordering::SeqCst)
}

/// `Host.ctx_registry_size! : () => U64` — live request stores (debug).
#[unsafe(no_mangle)]
pub extern "C" fn roc_ctx_registry_size() -> u64 {
    poison_current_ctx();
    ctx_registry().lock().expect("ctx registry poisoned").len() as u64
}

/// `Host.panic_test! : () => {}` — test-only host panic, to prove the Drop
/// guard tears the request down on unwind. "C-unwind" so the panic legally
/// crosses this boundary; whether it survives the Roc frames above is exactly
/// what the coeffect teardown test measures.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn roc_panic_test() {
    panic!("panic_test!: deliberate host panic (coeffect teardown test)");
}

/// `Host.memo_get! : U64, Str => Try(Box(({} => a)), [Miss])`.
///
/// On hit we `incref` the stored thunk before returning it, donating a fresh
/// reference for Roc to consume (via `Box.unbox` + call) while the store
/// keeps its own. On miss — or if the ctx has no registry entry, which cannot
/// happen for an in-flight request — we return the zero-payload `Err`
/// (`[Miss]`).
#[unsafe(no_mangle)]
pub extern "C" fn roc_memo_get(ctx: u64, key: abi::RocStr) -> abi::HostMemoGetResult {
    let host = roc_host();
    let name = String::from_utf8_lossy(key.as_slice()).into_owned();
    unsafe { key.decref(host) };

    let registry = ctx_registry().lock().expect("ctx registry poisoned");
    let stored = match registry.get(&ctx) {
        Some(state) => state.memo.get(&name).copied(),
        None => {
            tracing::error!("memo_get!({ctx}, {name:?}): no registry entry for ctx");
            None
        }
    };
    match stored {
        Some(raw) => {
            let thunk = raw as abi::RocErasedCallable;
            unsafe { abi::incref_erased_callable(thunk, 1) };
            abi::HostMemoGetResult {
                tag: abi::HostMemoGetResultTag::Ok,
                payload: abi::HostMemoGetResultPayload {
                    ok: ManuallyDrop::new(thunk),
                },
            }
        }
        None => abi::HostMemoGetResult {
            tag: abi::HostMemoGetResultTag::Err,
            payload: abi::HostMemoGetResultPayload { err: [] },
        },
    }
}

/// `Host.memo_put! : U64, Str, Box(({} => a)) => {}`.
///
/// Takes ownership of one reference to `boxed` (the glue ownership contract
/// for hosted args) and keeps it alive by storing the raw pointer. A same-key
/// re-registration displaces the old thunk, whose store-held reference we
/// must release (unconditionally: even if old == new, the store received TWO
/// transferred references but can only record one entry). If the ctx has no
/// registry entry (teardown already ran — cannot happen in-request), the
/// thunk would leak, so release it and log loudly.
#[unsafe(no_mangle)]
pub extern "C" fn roc_memo_put(ctx: u64, key: abi::RocStr, boxed: abi::RocErasedCallable) {
    let host = roc_host();
    let name = String::from_utf8_lossy(key.as_slice()).into_owned();
    unsafe { key.decref(host) };

    let displaced = {
        let mut registry = ctx_registry().lock().expect("ctx registry poisoned");
        match registry.get_mut(&ctx) {
            Some(state) => {
                LIVE_MEMO_BOXES.fetch_add(1, Ordering::SeqCst);
                state.memo.insert(name, boxed as usize)
            }
            None => {
                // Teardown already ran; the transferred reference must still
                // be released (outside the lock, like the displaced case).
                // Balance the gauge for release_stored_thunk's decrement.
                tracing::error!(
                    "memo_put!({ctx}, {name:?}): no registry entry for ctx; releasing thunk"
                );
                LIVE_MEMO_BOXES.fetch_add(1, Ordering::SeqCst);
                Some(boxed as usize)
            }
        }
    };
    if let Some(old) = displaced {
        release_stored_thunk(old);
    }
}

/// `Host.memo_live_boxes! : () => U64` — store-held boxes (debug), for
/// teardown-balance assertions.
#[unsafe(no_mangle)]
pub extern "C" fn roc_memo_live_boxes() -> u64 {
    poison_current_ctx();
    LIVE_MEMO_BOXES.load(Ordering::SeqCst)
}
