//! sqlgen library surface — the analyzer (and, from Phase 4, the scanner and
//! emitter) so both the binary and the integration-test battery share them.

pub mod analyze;
pub mod schema;

use rusqlite::Connection;

/// Hard-pin the analyzer to the SQLite build the host links (plan decision 8).
pub const EXPECTED_SQLITE_PREFIX: &str = "3.46.";

/// Assert the linked SQLite matches the pin the analyzer was validated against.
pub fn assert_sqlite_version() {
    let version = rusqlite::version();
    assert!(
        version.starts_with(EXPECTED_SQLITE_PREFIX),
        "sqlgen pinned to SQLite {EXPECTED_SQLITE_PREFIX}x, linked {version} — re-validate the analyzer against the new opcode set before bumping"
    );
}

/// An in-memory connection with the RealWorld schema applied, for analysis.
pub fn open_schema() -> Connection {
    let conn = Connection::open_in_memory().expect("open :memory:");
    for ddl in schema::REALWORLD_SCHEMA {
        conn.execute(ddl, [])
            .unwrap_or_else(|e| panic!("schema apply failed for {ddl:?}: {e}"));
    }
    conn
}
