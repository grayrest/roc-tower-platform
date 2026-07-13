//! Line-oriented scanner for `## @`-marked SQL in Roc source.
//!
//! No Roc parser: the scanner recognizes four markers, each immediately
//! followed by a top-level `name =` binding whose value is a string literal
//! (a single `"..."` or a `\\` multiline block):
//!
//! ```roc
//! ## @schema
//! schema_users =
//!     \\CREATE TABLE users (id INTEGER PRIMARY KEY, ...)
//!
//! ## @fragment article_select
//! article_select =
//!     \\SELECT a.id, ... FROM articles a JOIN users u ON u.id = a.author_id
//!
//! ## @sql one
//! load_article =
//!     \\${article_select} WHERE a.slug = :slug
//!
//! ## @params limit: I64, offset: I64
//! ## @sql
//! list_page =
//!     \\SELECT ... LIMIT :limit OFFSET :offset
//! ```
//!
//! `${Name}` references splice a same-module `@fragment` before analysis.
//! `@sql` queries must use named `:params` (a bare `?` is a scan error), so the
//! generated function can take a record argument.

/// A named type override from `## @params name: Type, ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamAnnot {
    pub name: String,
    pub ty: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub name: String,
    pub sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The binding name; becomes the generated function name.
    pub name: String,
    /// SQL with fragments already spliced.
    pub sql: String,
    /// `## @sql one` — return the first row (else the whole list).
    pub one: bool,
    /// `## @params` overrides, if present.
    pub params: Vec<ParamAnnot>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scanned {
    /// Schema DDL statements in declaration order.
    pub schema: Vec<String>,
    pub fragments: Vec<Fragment>,
    pub queries: Vec<Query>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Marker {
    Schema,
    Fragment(String),
    Sql { one: bool },
}

/// Scan one Roc source file's marked items.
pub fn scan(source: &str) -> Result<Scanned, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    let mut pending_marker: Option<Marker> = None;
    let mut pending_params: Vec<ParamAnnot> = Vec::new();
    let mut scanned = Scanned::default();

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        if let Some(rest) = trimmed.strip_prefix("## @params") {
            pending_params = parse_params(rest)?;
            i += 1;
            continue;
        }
        if let Some(marker) = parse_marker(trimmed)? {
            pending_marker = Some(marker);
            i += 1;
            continue;
        }
        // A blank/comment line does not clear a pending marker (the `## @sql`
        // and `name =` may be separated by the doc comment for the binding).
        if pending_marker.is_some() && is_binding(trimmed) {
            let (name, sql, next) = read_binding(&lines, i)?;
            let marker = pending_marker.take().unwrap();
            let params = std::mem::take(&mut pending_params);
            match marker {
                Marker::Schema => {
                    if !params.is_empty() {
                        return Err(format!("@params is not valid on @schema ({name})"));
                    }
                    // A schema binding may hold several statements separated by
                    // `;` — split so each applies cleanly.
                    for stmt in split_statements(&sql) {
                        scanned.schema.push(stmt);
                    }
                }
                Marker::Fragment(frag_name) => {
                    let spliced = splice_fragments(&sql, &scanned.fragments)?;
                    scanned.fragments.push(Fragment { name: frag_name, sql: spliced });
                }
                Marker::Sql { one } => {
                    let spliced = splice_fragments(&sql, &scanned.fragments)?;
                    if spliced.contains('?') {
                        return Err(format!(
                            "@sql query {name:?} uses a positional `?`; @sql requires named :params"
                        ));
                    }
                    scanned.queries.push(Query { name, sql: spliced, one, params });
                }
            }
            i = next;
            continue;
        }
        i += 1;
    }
    Ok(scanned)
}

fn parse_marker(trimmed: &str) -> Result<Option<Marker>, String> {
    let Some(rest) = trimmed.strip_prefix("## @") else {
        return Ok(None);
    };
    let rest = rest.trim();
    if rest == "schema" {
        Ok(Some(Marker::Schema))
    } else if let Some(name) = rest.strip_prefix("fragment") {
        let name = name.trim();
        if name.is_empty() {
            Err("@fragment requires a name".to_string())
        } else {
            Ok(Some(Marker::Fragment(name.to_string())))
        }
    } else if rest == "sql" {
        Ok(Some(Marker::Sql { one: false }))
    } else if rest == "sql one" {
        Ok(Some(Marker::Sql { one: true }))
    } else if rest.starts_with("params") {
        // handled by the caller
        Ok(None)
    } else {
        // Not one of ours (some other `## @…` doc convention) — ignore.
        Ok(None)
    }
}

fn parse_params(rest: &str) -> Result<Vec<ParamAnnot>, String> {
    rest.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (name, ty) = pair
                .split_once(':')
                .ok_or_else(|| format!("@params entry {pair:?} is not `name: Type`"))?;
            Ok(ParamAnnot {
                name: name.trim().to_string(),
                ty: ty.trim().to_string(),
            })
        })
        .collect()
}

/// Is this line a top-level binding `name =` (not indented continuation)?
fn is_binding(trimmed: &str) -> bool {
    // `name = ...` or `name =` at end of line; name is an identifier.
    if let Some((lhs, _)) = trimmed.split_once('=') {
        let lhs = lhs.trim();
        !lhs.is_empty()
            && lhs.chars().all(|c| c.is_alphanumeric() || c == '_')
            && lhs.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
    } else {
        false
    }
}

/// Read a binding starting at line `i`: its name and string value (single-line
/// `"..."` or a `\\` multiline block). Returns (name, sql, next_line_index).
fn read_binding(lines: &[&str], i: usize) -> Result<(String, String, usize), String> {
    let line = lines[i];
    let (lhs, rhs) = line.split_once('=').unwrap();
    let name = lhs.trim().to_string();
    let rhs = rhs.trim();

    // Single-line: name = "..."
    if let Some(inner) = rhs.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Ok((name, unescape(inner), i + 1));
    }

    // Multiline `\\` block: collect following lines whose trimmed form starts
    // with `\\`. The block may start on the same line (rhs) or the next lines.
    let mut sql_lines: Vec<String> = Vec::new();
    if let Some(content) = rhs.strip_prefix("\\\\") {
        sql_lines.push(content.to_string());
    } else if !rhs.is_empty() && !rhs.starts_with("\\\\") {
        // Some other single-line expression we don't model; skip as no SQL.
        return Ok((name, rhs.trim_matches('"').to_string(), i + 1));
    }
    let mut j = i + 1;
    while j < lines.len() {
        let t = lines[j].trim_start();
        if let Some(content) = t.strip_prefix("\\\\") {
            sql_lines.push(content.to_string());
            j += 1;
        } else {
            break;
        }
    }
    Ok((name, sql_lines.join("\n"), j))
}

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\\"", "\"").replace("\\\\", "\\")
}

/// Replace every `${Name}` with the SQL of the same-named fragment.
fn splice_fragments(sql: &str, fragments: &[Fragment]) -> Result<String, String> {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let end = sql[i + 2..]
                .find('}')
                .ok_or_else(|| "unterminated ${...} fragment reference".to_string())?;
            let name = &sql[i + 2..i + 2 + end];
            let frag = fragments
                .iter()
                .find(|f| f.name == name)
                .ok_or_else(|| format!("unknown fragment ${{{name}}} (fragments must be declared before use)"))?;
            out.push_str(&frag.sql);
            i = i + 2 + end + 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

/// Split a schema binding on `;` into individual statements, dropping blanks.
fn split_statements(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
