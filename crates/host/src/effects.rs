//! Hosted function implementations — the effects available to Roc code.
//!
//! Ownership contract (see the generated glue header): hosted functions own
//! their refcounted arguments and must release them; returned values transfer
//! ownership to Roc.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::abi;
use crate::roc_call::{roc_kv_list, roc_str};
use crate::roc_host;
use crate::types::RocSqliteResultOk;

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
unsafe fn decref_list_with<T: Copy>(list: abi::RocList<T>, drop_item: impl Fn(T)) {
    if list.has_one_ref() {
        for item in list.allocation_items() {
            drop_item(*item);
        }
    }
    unsafe { list.decref(roc_host()) };
}

type SharedConnection = Arc<Mutex<rusqlite::Connection>>;

fn sqlite_connections() -> &'static Mutex<HashMap<String, SharedConnection>> {
    static CONNECTIONS: OnceLock<Mutex<HashMap<String, SharedConnection>>> = OnceLock::new();
    CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
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

/// `Host.now_ms! : () => U64`
#[unsafe(no_mangle)]
pub extern "C" fn roc_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// `Host.random_bytes! : U64 => List(U8)`
#[unsafe(no_mangle)]
pub extern "C" fn roc_random_bytes(count: u64) -> abi::RocListWith<u8, false> {
    let count = count.min(MAX_RANDOM_BYTES) as usize;
    let mut buffer = vec![0u8; count];
    if let Err(err) = getrandom::fill(&mut buffer) {
        // A failing OS entropy source is not something the app can handle.
        panic!("random_bytes: OS entropy source failed: {err}");
    }
    unsafe { abi::RocListWith::<u8, false>::from_slice(&buffer, roc_host()) }
}

// ---------------------------------------------------------------------------
// Outbound HTTP.
// ---------------------------------------------------------------------------

/// `Host.http_send!` — send an HTTP request, driven on the server's runtime.
#[unsafe(no_mangle)]
pub extern "C" fn roc_http_send(args: abi::HostHttpSendArgs) -> abi::HostHttpSendResult {
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
                ok: ManuallyDrop::new(crate::types::RocResponse {
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

fn send_request(
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

enum SqlCell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// `Host.sqlite_exec!` — run one statement against a (cached) connection.
#[unsafe(no_mangle)]
pub extern "C" fn roc_sqlite_exec(args: abi::HostSqliteExecArgs) -> abi::HostSqliteExecResult {
    let host = roc_host();
    let db = String::from_utf8_lossy(args.db.as_slice()).into_owned();
    let sql = String::from_utf8_lossy(args.sql.as_slice()).into_owned();
    let params: Vec<rusqlite::types::Value> = args
        .params
        .as_slice()
        .iter()
        .map(sql_param_to_rusqlite)
        .collect();
    unsafe {
        args.db.decref(host);
        decref_list_with(args.params, |value| value.decref(host));
        args.sql.decref(host);
    }

    match execute_statement(&db, &sql, params) {
        Ok((columns, rows)) => abi::HostSqliteExecResult {
            tag: abi::HostSqliteExecResultTag::Ok,
            payload: abi::HostSqliteExecResultPayload {
                ok: ManuallyDrop::new(build_sqlite_ok(&columns, rows)),
            },
        },
        Err(message) => abi::HostSqliteExecResult {
            tag: abi::HostSqliteExecResultTag::Err,
            payload: abi::HostSqliteExecResultPayload {
                err: ManuallyDrop::new(roc_str(&message)),
            },
        },
    }
}

fn sql_param_to_rusqlite(value: &abi::HostSqlValue) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match value.tag {
        abi::HostSqlValueTag::Null => Value::Null,
        abi::HostSqlValueTag::Integer => Value::Integer(value.payload_integer()),
        abi::HostSqlValueTag::Real => Value::Real(value.payload_real()),
        abi::HostSqlValueTag::Text => Value::Text(
            String::from_utf8_lossy(value.payload_text().as_slice()).into_owned(),
        ),
        abi::HostSqlValueTag::Blob => Value::Blob(value.payload_blob().as_slice().to_vec()),
    }
}

fn open_connection(db: &str) -> Result<SharedConnection, String> {
    let mut connections = sqlite_connections()
        .lock()
        .map_err(|_| "sqlite connection cache poisoned".to_string())?;
    if let Some(connection) = connections.get(db) {
        return Ok(connection.clone());
    }
    let connection = rusqlite::Connection::open(db)
        .map_err(|err| format!("cannot open database {db:?}: {err}"))?;
    let shared = Arc::new(Mutex::new(connection));
    connections.insert(db.to_owned(), shared.clone());
    Ok(shared)
}

fn execute_statement(
    db: &str,
    sql: &str,
    params: Vec<rusqlite::types::Value>,
) -> Result<(Vec<String>, Vec<Vec<SqlCell>>), String> {
    let shared = open_connection(db)?;
    let connection = shared
        .lock()
        .map_err(|_| "sqlite connection poisoned".to_string())?;
    let mut statement = connection.prepare(sql).map_err(|err| err.to_string())?;
    let column_count = statement.column_count();

    if column_count == 0 {
        statement
            .execute(rusqlite::params_from_iter(params))
            .map_err(|err| err.to_string())?;
        return Ok((Vec::new(), Vec::new()));
    }

    let columns: Vec<String> = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let mut rows_out = Vec::new();
    let mut rows = statement
        .query(rusqlite::params_from_iter(params))
        .map_err(|err| err.to_string())?;
    while let Some(row) = rows.next().map_err(|err| err.to_string())? {
        let mut cells = Vec::with_capacity(column_count);
        for index in 0..column_count {
            use rusqlite::types::ValueRef;
            cells.push(match row.get_ref(index).map_err(|err| err.to_string())? {
                ValueRef::Null => SqlCell::Null,
                ValueRef::Integer(value) => SqlCell::Integer(value),
                ValueRef::Real(value) => SqlCell::Real(value),
                ValueRef::Text(bytes) => {
                    SqlCell::Text(String::from_utf8_lossy(bytes).into_owned())
                }
                ValueRef::Blob(bytes) => SqlCell::Blob(bytes.to_vec()),
            });
        }
        rows_out.push(cells);
    }
    Ok((columns, rows_out))
}

fn build_sqlite_ok(columns: &[String], rows: Vec<Vec<SqlCell>>) -> RocSqliteResultOk {
    let host = roc_host();
    unsafe {
        let column_list = abi::RocList::<abi::RocStr>::allocate(columns.len(), host);
        for (index, column) in columns.iter().enumerate() {
            column_list.elements.add(index).write(roc_str(column));
        }

        let row_list =
            abi::RocList::<abi::RocList<abi::HostSqlValue>>::allocate(rows.len(), host);
        for (row_index, row) in rows.iter().enumerate() {
            let cell_list = abi::RocList::<abi::HostSqlValue>::allocate(row.len(), host);
            for (cell_index, cell) in row.iter().enumerate() {
                cell_list.elements.add(cell_index).write(sql_cell_to_roc(cell));
            }
            row_list.elements.add(row_index).write(cell_list);
        }

        RocSqliteResultOk {
            columns: column_list,
            rows: row_list,
        }
    }
}

fn sql_cell_to_roc(cell: &SqlCell) -> abi::HostSqlValue {
    match cell {
        SqlCell::Null => abi::HostSqlValue {
            tag: abi::HostSqlValueTag::Null,
            payload: abi::HostSqlValuePayload { null: [] },
        },
        SqlCell::Integer(value) => abi::HostSqlValue {
            tag: abi::HostSqlValueTag::Integer,
            payload: abi::HostSqlValuePayload {
                integer: ManuallyDrop::new(*value),
            },
        },
        SqlCell::Real(value) => abi::HostSqlValue {
            tag: abi::HostSqlValueTag::Real,
            payload: abi::HostSqlValuePayload {
                real: ManuallyDrop::new(*value),
            },
        },
        SqlCell::Text(value) => abi::HostSqlValue {
            tag: abi::HostSqlValueTag::Text,
            payload: abi::HostSqlValuePayload {
                text: ManuallyDrop::new(roc_str(value)),
            },
        },
        SqlCell::Blob(value) => abi::HostSqlValue {
            tag: abi::HostSqlValueTag::Blob,
            payload: abi::HostSqlValuePayload {
                blob: ManuallyDrop::new(unsafe {
                    abi::RocListWith::<u8, false>::from_slice(value, roc_host())
                }),
            },
        },
    }
}
