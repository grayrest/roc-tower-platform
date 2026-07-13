//! sqlgen — typed-SQL codegen for tower-platform.
//!
//!   sqlgen gen   <app-dir>   regenerate `<File>Gen.roc` for each source file
//!                            with `## @sql` queries
//!   sqlgen check <app-dir>   regenerate to a temp buffer and diff against the
//!                            committed `<File>Gen.roc`; nonzero exit on drift
//!
//! Schema (`## @schema`) may be declared in any file; its union (in file +
//! declaration order) is applied to an in-memory database before analysis.
//! Fragments (`## @fragment`) are same-module. See plan
//! `plans/2026-07-13-typed-sql.md`.

use sqlgen::scan::Scanned;
use sqlgen::{assert_sqlite_version, emit, scan};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    assert_sqlite_version();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => run(args.get(2), Mode::Write),
        Some("check") => run(args.get(2), Mode::Check),
        _ => {
            eprintln!("usage: sqlgen (gen|check) <app-dir>");
            ExitCode::FAILURE
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Write,
    Check,
}

fn run(dir: Option<&String>, mode: Mode) -> ExitCode {
    let Some(dir) = dir else {
        eprintln!("usage: sqlgen (gen|check) <app-dir>");
        return ExitCode::FAILURE;
    };
    match generate(Path::new(dir), mode) {
        Ok(drift) if mode == Mode::Check && drift => {
            eprintln!("sqlgen check: generated code is stale — run `just sqlgen`");
            ExitCode::FAILURE
        }
        Ok(_) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sqlgen: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Scan every `.roc` in `dir`, collect the global schema, and emit a Gen module
/// for each file with queries. In check mode, returns whether any Gen file
/// drifted; in write mode, writes them and returns false.
fn generate(dir: &Path, mode: Mode) -> Result<bool, String> {
    let mut sources: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "roc"))
        .filter(|p| !is_generated(p))
        .collect();
    sources.sort();

    // Pass 1: scan every file; accumulate the global schema in file order.
    let mut scans: Vec<(PathBuf, Scanned)> = Vec::new();
    let mut schema: Vec<String> = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let scanned = scan::scan(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        schema.extend(scanned.schema.iter().cloned());
        scans.push((path.clone(), scanned));
    }

    // Pass 2: emit a Gen module per file that has queries.
    let mut drift = false;
    for (path, scanned) in &scans {
        if scanned.queries.is_empty() {
            continue;
        }
        let module = module_name(path);
        let merged = Scanned {
            schema: schema.clone(),
            fragments: scanned.fragments.clone(),
            queries: scanned.queries.clone(),
        };
        let generated = emit::emit_module(&module, &merged)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let out_path = path.with_file_name(format!("{module}Gen.roc"));
        match mode {
            Mode::Write => {
                std::fs::write(&out_path, &generated)
                    .map_err(|e| format!("write {}: {e}", out_path.display()))?;
                eprintln!("sqlgen: wrote {}", out_path.display());
            }
            Mode::Check => {
                let existing = std::fs::read_to_string(&out_path).unwrap_or_default();
                if existing != generated {
                    eprintln!("sqlgen: {} is stale", out_path.display());
                    drift = true;
                }
            }
        }
    }
    Ok(drift)
}

fn is_generated(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.ends_with("Gen"))
}

fn module_name(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("App").to_string()
}
