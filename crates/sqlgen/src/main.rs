//! sqlgen — typed-SQL codegen for tower-platform.
//!
//! Phase 1 (this spike): prove the VDBE bytecode analyzer types the hardest
//! realworld query (`article_select`) correctly. See plan
//! `plans/2026-07-13-typed-sql.md`.

use sqlgen::{analyze, assert_sqlite_version, open_schema, schema};

fn main() {
    assert_sqlite_version();
    eprintln!("sqlgen spike — SQLite {}", rusqlite::version());

    let conn = open_schema();
    let analyzer = analyze::Analyzer::new(&conn);
    let by_slug = format!("{} WHERE a.slug = :slug", schema::ARTICLE_SELECT);
    match analyzer.analyze(&by_slug) {
        Ok(a) => {
            println!("readonly={}  params={:?}", a.readonly, a.params);
            for c in &a.columns {
                let q = if c.nullable { "?" } else { " " };
                println!("  {:<16} {:?}{}", c.name, c.ty, q);
            }
        }
        Err(e) => {
            eprintln!("ANALYZE FAILED: {e}");
            std::process::exit(1);
        }
    }
}
