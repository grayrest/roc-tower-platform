//! Phase 1 kill-criterion battery. Every realworld read query, hardest first,
//! plus LEFT-JOIN nullability and write detection. If `article_select` does
//! not type exactly as asserted here, Phase 1's kill criterion has fired and
//! the analyzer pivots to decltype + mandatory annotations.

use sqlgen::analyze::{Analyzer, SqlType};
use sqlgen::{assert_sqlite_version, open_schema, schema};

/// (name, type, nullable) triples for terse assertions.
fn shape(sql: &str) -> Vec<(String, SqlType, bool)> {
    let conn = open_schema();
    let a = Analyzer::new(&conn)
        .analyze(sql)
        .unwrap_or_else(|e| panic!("analyze failed for {sql:?}: {e}"));
    a.columns
        .into_iter()
        .map(|c| (c.name, c.ty, c.nullable))
        .collect()
}

fn col(name: &str, ty: SqlType, nullable: bool) -> (String, SqlType, bool) {
    (name.to_string(), ty, nullable)
}

#[test]
fn sqlite_version_pinned() {
    assert_sqlite_version();
}

/// THE KILL CRITERION: article_select's four expression columns.
#[test]
fn article_select_by_slug() {
    use SqlType::*;
    let sql = format!("{} WHERE a.slug = :slug", schema::ARTICLE_SELECT);
    let got = shape(&sql);
    let want = vec![
        col("id", Integer, false),
        col("author_id", Integer, false),
        col("slug", Text, false),
        col("title", Text, false),
        col("description", Text, false),
        col("body", Text, false),
        col("created_at", Text, false),
        col("updated_at", Text, false),
        col("username", Text, false),
        col("bio", Text, true),
        col("image", Text, true),
        col("favorites_count", Integer, false),
        col("favorited", Integer, false),
        col("following", Integer, false),
        col("tag_list", Text, true),
    ];
    assert_eq!(got, want);
}

/// The same fragment spliced with a dynamic page WHERE + LIMIT/OFFSET params.
#[test]
fn article_list_page() {
    let sql = format!(
        "{} WHERE u.username = :author ORDER BY a.id DESC LIMIT :limit OFFSET :offset",
        schema::ARTICLE_SELECT
    );
    let got = shape(&sql);
    // Same 15 columns as by_slug; assert the expression columns specifically.
    assert_eq!(got[11], col("favorites_count", SqlType::Integer, false));
    assert_eq!(got[12], col("favorited", SqlType::Integer, false));
    assert_eq!(got[13], col("following", SqlType::Integer, false));
    assert_eq!(got[14], col("tag_list", SqlType::Text, true));
}

/// load_profile!: EXISTS following flag + the nullable bio/image.
#[test]
fn load_profile() {
    use SqlType::*;
    let sql = "SELECT u.id, u.username, u.bio, u.image, \
        EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = u.id) AS following \
        FROM users u WHERE u.username = :username";
    let got = shape(sql);
    assert_eq!(
        got,
        vec![
            col("id", Integer, false),
            col("username", Text, false),
            col("bio", Text, true),
            col("image", Text, true),
            col("following", Integer, false),
        ]
    );
}

/// load_comments!: joined author profile + following flag.
#[test]
fn load_comments() {
    use SqlType::*;
    let sql = "SELECT c.id, c.body, c.created_at, c.updated_at, u.username, u.bio, u.image, \
        EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = c.author_id) AS following \
        FROM comments c JOIN users u ON u.id = c.author_id WHERE c.article_id = :article ORDER BY c.id";
    let got = shape(sql);
    assert_eq!(
        got,
        vec![
            col("id", Integer, false),
            col("body", Text, false),
            col("created_at", Text, false),
            col("updated_at", Text, false),
            col("username", Text, false),
            col("bio", Text, true),
            col("image", Text, true),
            col("following", Integer, false),
        ]
    );
}

/// load_article_meta! / load_comment!: bare id + fk lookups.
#[test]
fn article_meta() {
    use SqlType::*;
    let got = shape("SELECT id, author_id FROM articles WHERE slug = :slug");
    assert_eq!(
        got,
        vec![col("id", Integer, false), col("author_id", Integer, false)]
    );
}

/// all_tags!: a plain projection.
#[test]
fn all_tags() {
    let got = shape("SELECT DISTINCT tag FROM article_tags ORDER BY tag");
    assert_eq!(got, vec![col("tag", SqlType::Text, false)]);
}

/// Auth token lookup: cross-table read on every authed request.
#[test]
fn token_lookup() {
    use SqlType::*;
    let got = shape(
        "SELECT u.id, u.username FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.token = :token",
    );
    assert_eq!(
        got,
        vec![col("id", Integer, false), col("username", Text, false)]
    );
}

/// A COUNT aggregate query (the articlesCount companion) — count is NOT NULL.
#[test]
fn count_query() {
    let got = shape("SELECT COUNT(*) AS n FROM articles a JOIN users u ON u.id = a.author_id");
    assert_eq!(got, vec![col("n", SqlType::Integer, false)]);
}

/// LEFT JOIN: the right side's NOT NULL columns become nullable in the result
/// (no matching row → NULLs). The realworld reads don't use LEFT JOIN, but the
/// analyzer must not mistype it as NOT NULL.
#[test]
fn left_join_nullability() {
    use SqlType::*;
    // users LEFT JOIN articles: a.title is declared NOT NULL but a user with no
    // articles yields NULL title.
    let got = shape(
        "SELECT u.username, a.title FROM users u LEFT JOIN articles a ON a.author_id = u.id",
    );
    assert_eq!(got[0], col("username", Text, false));
    assert_eq!(
        got[1],
        col("title", Text, true),
        "LEFT-joined NOT NULL column must be nullable"
    );
}

/// Writes are detected as non-readonly; app writes use pre-reserved ids, so no
/// RETURNING is needed here.
#[test]
fn insert_is_write() {
    let conn = open_schema();
    let a = Analyzer::new(&conn)
        .analyze("INSERT INTO users (id, email, username, password_hash) VALUES (:id, :email, :username, :hash)")
        .expect("analyze insert");
    assert!(!a.readonly, "INSERT must be detected as a write");
    let names: Vec<_> = a.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["id", "email", "username", "hash"]);
}

/// The reserved_id! coeffect: UPDATE ... RETURNING is a write that yields a
/// row. It stays hand-written on Sql.roc (the emitter rejects RETURNING
/// writes), so the analyzer need only detect it is a non-readonly statement
/// that surfaces a column — its value flows through a RETURNING ephemeral, so
/// exact typing is best-effort, not asserted.
#[test]
fn update_returning_is_write_with_row() {
    let conn = open_schema();
    conn.execute("CREATE TABLE counters (name TEXT PRIMARY KEY, v INTEGER NOT NULL)", [])
        .unwrap();
    let a = Analyzer::new(&conn)
        .analyze("UPDATE counters SET v = v + 1 WHERE name = 'reserved_id' RETURNING v")
        .expect("analyze update returning");
    assert!(!a.readonly, "UPDATE RETURNING is a write");
    assert_eq!(a.columns.len(), 1, "one RETURNING column");
    assert_eq!(a.columns[0].name, "v");
}
