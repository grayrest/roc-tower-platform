//! B0 coverage + perf spike (GATE).
//!
//! Standalone binary that decides whether the rusqlite -> Turso runtime swap is
//! viable, BEFORE any of B1–B4. It exercises Turso (`turso_sdk_kit::rsapi`, the
//! synchronous `async_io: false` stepping mode — the surface the cursor FFI
//! would map onto) against the RealWorld schema and its real queries, then the
//! operational properties a production swap depends on: writer contention,
//! durability, file compatibility, pragmas, and constraint-error classification,
//! plus a microbench against rusqlite.
//!
//! Prints a section-by-section report and a final GATE verdict. Pre-committed
//! no-go: any live query unsupported/incorrect, writer contention with no
//! workable mitigation, or the article_select microbench exceeding 1.5x
//! rusqlite. Run: `cargo run -p turso-spike`.

use std::time::{Duration, Instant};

use turso_sdk_kit::rsapi::{
    TursoConnection, TursoDatabase, TursoDatabaseConfig, TursoStatusCode, Value,
};

// --- RealWorld schema (verbatim from examples/realworld/main.roc @schema) -----

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, bio TEXT, image TEXT)",
    "CREATE TABLE IF NOT EXISTS tokens (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id))",
    "CREATE TABLE IF NOT EXISTS follows (follower_id INTEGER NOT NULL, followed_id INTEGER NOT NULL, PRIMARY KEY (follower_id, followed_id))",
    "CREATE TABLE IF NOT EXISTS articles (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, author_id INTEGER NOT NULL REFERENCES users(id), created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS article_tags (article_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY (article_id, tag))",
    "CREATE TABLE IF NOT EXISTS favorites (user_id INTEGER NOT NULL, article_id INTEGER NOT NULL, PRIMARY KEY (user_id, article_id))",
    "CREATE TABLE IF NOT EXISTS comments (id INTEGER PRIMARY KEY, article_id INTEGER NOT NULL REFERENCES articles(id), author_id INTEGER NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS idx_articles_author ON articles(author_id)",
    "CREATE INDEX IF NOT EXISTS idx_article_tags_tag ON article_tags(tag)",
    "CREATE INDEX IF NOT EXISTS idx_favorites_article ON favorites(article_id, user_id)",
    "CREATE INDEX IF NOT EXISTS idx_comments_article ON comments(article_id)",
];

// The article summary fragment sqlgen splices into by-slug and list/feed
// (examples/realworld/Db.roc @fragment article_json_summary). `:viewer` is used
// FOUR times across two EXISTS subqueries — the mixed-reuse binding is exactly
// the silent-wrong-data risk this spike must rule out.
const ARTICLE_JSON_SUMMARY: &str = "json_object(\
  'slug', a.slug, 'title', a.title, 'description', a.description, \
  'tagList', (SELECT json_group_array(tag) FROM (SELECT at2.tag FROM article_tags at2 WHERE at2.article_id = a.id ORDER BY at2.tag)), \
  'createdAt', a.created_at, 'updatedAt', a.updated_at, \
  'favorited', json(iif(EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = :viewer), 'true', 'false')), \
  'favoritesCount', (SELECT COUNT(*) FROM favorites f WHERE f.article_id = a.id), \
  'author', json_object('username', u.username, 'bio', u.bio, 'image', u.image, \
    'following', json(iif(EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = a.author_id), 'true', 'false'))))";

fn article_json_by_slug() -> String {
    format!(
        "SELECT json_object('article', json_patch({ARTICLE_JSON_SUMMARY}, json_object('body', a.body))) \
         FROM articles a JOIN users u ON u.id = a.author_id WHERE a.slug = :slug"
    )
}

const COMMENTS_JSON: &str = "SELECT json_object('comments', COALESCE((\
    SELECT json_group_array(json_object('id', c.id, 'createdAt', c.created_at, 'updatedAt', c.updated_at, 'body', c.body, \
        'author', json_object('username', u.username, 'bio', u.bio, 'image', u.image, \
            'following', json(iif(EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = c.author_id), 'true', 'false'))))) \
    FROM comments c JOIN users u ON u.id = c.author_id WHERE c.article_id = :article_id ORDER BY c.id), json('[]')))";

// The dynamic article list (Db.roc splices the summary into a json_group_array).
fn article_list() -> String {
    format!(
        "SELECT json_object('articles', COALESCE(json_group_array(json({ARTICLE_JSON_SUMMARY})), json('[]')), 'articlesCount', (SELECT COUNT(*) FROM articles)) \
         FROM articles a JOIN users u ON u.id = a.author_id ORDER BY a.id DESC LIMIT :limit OFFSET :offset"
    )
}

// --- Turso helpers ------------------------------------------------------------

fn open(path: &str) -> Result<std::sync::Arc<TursoDatabase>, String> {
    let db = TursoDatabase::new(TursoDatabaseConfig {
        path: path.to_string(),
        experimental_features: None,
        async_io: false,
        encryption: None,
        vfs: None,
        io: None,
        db_file: None,
    });
    let io = db.open().map_err(|e| format!("open {path}: {e:?}"))?;
    if io.is_io() {
        return Err("open returned Io under async_io=false".into());
    }
    Ok(db)
}

/// Run a statement with no result rows (DDL / INSERT / BEGIN / COMMIT).
fn exec(conn: &TursoConnection, sql: &str) -> Result<u64, String> {
    let mut stmt = conn.prepare_single(sql).map_err(|e| format!("prepare `{sql:.60}`: {e:?}"))?;
    let result = stmt.execute(None).map_err(|e| format!("exec `{sql:.60}`: {e:?}"))?;
    Ok(result.rows_changed)
}

/// Bind `(":name", Value)` pairs then read column 0 of the first row as text.
/// Returns None when the query yields no row.
fn query_text(conn: &TursoConnection, sql: &str, binds: &[(&str, Value)]) -> Result<Option<String>, String> {
    let mut stmt = conn.prepare_single(sql).map_err(|e| format!("prepare: {e:?}"))?;
    for (name, value) in binds {
        let pos = stmt.named_position(name).map_err(|e| format!("named_position {name}: {e:?}"))?;
        stmt.bind_positional(pos, value.clone()).map_err(|e| format!("bind {name}: {e:?}"))?;
    }
    let out = match stmt.step(None).map_err(|e| format!("step: {e:?}"))? {
        TursoStatusCode::Row => stmt.row_value(0).map_err(|e| format!("row_value: {e:?}"))?.to_text().map(str::to_owned),
        TursoStatusCode::Done => None,
        other => return Err(format!("unexpected status {other:?}")),
    };
    stmt.finalize(None).map_err(|e| format!("finalize: {e:?}"))?;
    Ok(out)
}

// --- Seed ---------------------------------------------------------------------

fn seed(conn: &TursoConnection) -> Result<(), String> {
    for sql in SCHEMA {
        exec(conn, sql)?;
    }
    // Two authors, one viewer.
    for (id, name) in [(1, "alice"), (2, "bob"), (3, "viewer")] {
        exec(conn, &format!(
            "INSERT INTO users (id, email, username, password_hash) VALUES ({id}, '{name}@x.com', '{name}', 'x')"
        ))?;
    }
    // Two articles by alice(1) and bob(2).
    for (id, slug, author) in [(10, "article-a", 1), (11, "article-b", 2)] {
        exec(conn, &format!(
            "INSERT INTO articles (id, slug, title, description, body, author_id, created_at, updated_at) \
             VALUES ({id}, '{slug}', 'T{id}', 'D{id}', 'Body {id}', {author}, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"
        ))?;
    }
    exec(conn, "INSERT INTO article_tags (article_id, tag) VALUES (10, 'zeta'), (10, 'alpha'), (11, 'beta')")?;
    // viewer(3) favorites article 10 and follows alice(1) but not bob(2).
    exec(conn, "INSERT INTO favorites (user_id, article_id) VALUES (3, 10)")?;
    exec(conn, "INSERT INTO follows (follower_id, followed_id) VALUES (3, 1)")?;
    // Comments on article 10 by alice(1) and bob(2).
    exec(conn, "INSERT INTO comments (id, article_id, author_id, body, created_at, updated_at) VALUES \
        (100, 10, 1, 'first', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'), \
        (101, 10, 2, 'second', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')")?;
    Ok(())
}

// --- Checks -------------------------------------------------------------------

struct Report {
    failures: Vec<String>,
}
impl Report {
    fn check(&mut self, name: &str, result: Result<(), String>) {
        match result {
            Ok(()) => println!("  ok   {name}"),
            Err(e) => {
                println!("  FAIL {name}: {e}");
                self.failures.push(format!("{name}: {e}"));
            }
        }
    }
}

/// Coverage: every RealWorld query shape prepares and runs, and the mixed
/// `:viewer` reuse produces flags that track the viewer (the flag-correctness
/// assertion) rather than silently defaulting.
fn check_coverage(conn: &TursoConnection, r: &mut Report) {
    // by-slug: viewer 3 favorited article 10 and follows its author (alice) ->
    // both favorited and following must be true; article 11's author (bob) is
    // not followed -> following false, favorited false.
    r.check("by-slug flags track the viewer", (|| {
        let a = query_text(conn, &article_json_by_slug(), &[(":slug", Value::build_text("article-a")), (":viewer", Value::from_i64(3))])?
            .ok_or("article-a returned no row")?;
        if !(a.contains("\"favorited\":true") && a.contains("\"following\":true")) {
            return Err(format!("expected favorited+following true for viewer 3 on article-a: {a}"));
        }
        let b = query_text(conn, &article_json_by_slug(), &[(":slug", Value::build_text("article-b")), (":viewer", Value::from_i64(3))])?
            .ok_or("article-b returned no row")?;
        if !(b.contains("\"favorited\":false") && b.contains("\"following\":false")) {
            return Err(format!("expected favorited+following false for viewer 3 on article-b: {b}"));
        }
        // Anonymous viewer (-1): everything false.
        let anon = query_text(conn, &article_json_by_slug(), &[(":slug", Value::build_text("article-a")), (":viewer", Value::from_i64(-1))])?
            .ok_or("anon returned no row")?;
        if anon.contains("\"favorited\":true") || anon.contains("\"following\":true") {
            return Err(format!("anon viewer should see no flags set: {anon}"));
        }
        Ok(())
    })());

    // tagList must be a sorted JSON array assembled by the correlated subquery.
    r.check("by-slug tagList is a sorted json array", (|| {
        let a = query_text(conn, &article_json_by_slug(), &[(":slug", Value::build_text("article-a")), (":viewer", Value::from_i64(-1))])?
            .ok_or("no row")?;
        if !a.contains("[\"alpha\",\"zeta\"]") {
            return Err(format!("expected sorted tagList [alpha,zeta]: {a}"));
        }
        Ok(())
    })());

    // comments: viewer 3 follows alice(author of comment 100) but not bob(101).
    r.check("comments following flag tracks the viewer", (|| {
        let doc = query_text(conn, COMMENTS_JSON, &[(":viewer", Value::from_i64(3)), (":article_id", Value::from_i64(10))])?
            .ok_or("comments returned no row")?;
        // comment 100 (alice, followed) true; 101 (bob, not) false.
        let trues = doc.matches("\"following\":true").count();
        let falses = doc.matches("\"following\":false").count();
        if trues != 1 || falses != 1 {
            return Err(format!("expected exactly one following:true and one false: {doc}"));
        }
        Ok(())
    })());

    // empty comments envelope for an article with none.
    r.check("comments empty envelope", (|| {
        let doc = query_text(conn, COMMENTS_JSON, &[(":viewer", Value::from_i64(-1)), (":article_id", Value::from_i64(11))])?
            .ok_or("no row")?;
        if doc != "{\"comments\":[]}" {
            return Err(format!("expected empty comments envelope, got: {doc}"));
        }
        Ok(())
    })());

    // dynamic list with LIMIT/OFFSET.
    r.check("article list json_group_array + limit/offset", (|| {
        let doc = query_text(conn, &article_list(), &[(":viewer", Value::from_i64(3)), (":limit", Value::from_i64(20)), (":offset", Value::from_i64(0))])?
            .ok_or("no row")?;
        if !doc.contains("\"articlesCount\":2") || !doc.contains("article-a") || !doc.contains("article-b") {
            return Err(format!("expected 2 articles in list: {doc}"));
        }
        Ok(())
    })());

    // A typed (non-json) multi-column read.
    r.check("typed comment_meta read", (|| {
        let sql = "SELECT c.id, c.author_id FROM comments c WHERE c.id = :comment_id AND c.article_id = :article_id";
        let mut stmt = conn.prepare_single(sql).map_err(|e| format!("{e:?}"))?;
        let p = stmt.named_position(":comment_id").map_err(|e| format!("{e:?}"))?;
        stmt.bind_positional(p, Value::from_i64(100)).map_err(|e| format!("{e:?}"))?;
        let p = stmt.named_position(":article_id").map_err(|e| format!("{e:?}"))?;
        stmt.bind_positional(p, Value::from_i64(10)).map_err(|e| format!("{e:?}"))?;
        match stmt.step(None).map_err(|e| format!("{e:?}"))? {
            TursoStatusCode::Row => {
                let id = stmt.row_value(0).map_err(|e| format!("{e:?}"))?.as_int();
                let author = stmt.row_value(1).map_err(|e| format!("{e:?}"))?.as_int();
                if id != Some(100) || author != Some(1) {
                    return Err(format!("expected (100,1) got ({id:?},{author:?})"));
                }
            }
            other => return Err(format!("expected a row, got {other:?}")),
        }
        Ok(())
    })());
}

/// A fresh temp-file path (Turso needs a real file for cross-connection
/// contention and for rusqlite to reopen it).
fn temp_db(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!("turso-spike-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    p.to_string_lossy().into_owned()
}

/// Writer-vs-writer: does a second connection's BEGIN IMMEDIATE honor
/// busy_timeout (wait) while the first holds the write lock, or fail fast? Either
/// way is workable (fast-fail just needs a host retry/single-writer mutex), so
/// this records behavior rather than gating on it.
fn check_contention(r: &mut Report) {
    r.check("writer-vs-writer BEGIN IMMEDIATE behavior", (|| {
        let path = temp_db("contend");
        let db_a = open(&path)?;
        let conn_a = db_a.connect().map_err(|e| format!("connect A: {e:?}"))?;
        seed(&conn_a)?;
        let db_b = open(&path)?;
        let conn_b = db_b.connect().map_err(|e| format!("connect B: {e:?}"))?;
        conn_b.set_busy_timeout(Duration::from_millis(750));

        // A takes the write lock and holds it.
        exec(&conn_a, "BEGIN IMMEDIATE")?;
        exec(&conn_a, "INSERT INTO favorites (user_id, article_id) VALUES (2, 11)")?;

        // B tries to take it; time how long before it yields.
        let start = Instant::now();
        let b_begin = exec(&conn_b, "BEGIN IMMEDIATE").and_then(|_| {
            exec(&conn_b, "INSERT INTO favorites (user_id, article_id) VALUES (1, 11)")
        });
        let waited = start.elapsed();
        let _ = exec(&conn_a, "COMMIT");
        let _ = std::fs::remove_file(&path);

        match b_begin {
            Ok(_) => Err("B acquired the write lock while A held it — no isolation".into()),
            Err(msg) => {
                let honored = waited >= Duration::from_millis(500);
                println!(
                    "        (B blocked {waited:?} then failed; busy_timeout {} → mitigation: {})",
                    if honored { "honored" } else { "NOT honored" },
                    if honored { "rely on busy_timeout" } else { "host single-writer mutex / retry loop" },
                );
                if msg.to_lowercase().contains("busy") || msg.to_lowercase().contains("lock") {
                    Ok(())
                } else {
                    Err(format!("unexpected error under contention: {msg}"))
                }
            }
        }
    })());
}

/// Durability + rollback safety: a Turso-written file reopens under rusqlite
/// (the bundled SQLite) with intact data and a clean integrity_check.
fn check_file_compat(r: &mut Report) {
    r.check("rusqlite reopens a turso-written file (rollback path)", (|| {
        let path = temp_db("compat");
        {
            let db = open(&path)?;
            let conn = db.connect().map_err(|e| format!("connect: {e:?}"))?;
            seed(&conn)?;
            exec(&conn, "PRAGMA wal_checkpoint(TRUNCATE)").ok();
            conn.close().map_err(|e| format!("close: {e:?}"))?;
        }
        let sq = rusqlite::Connection::open(&path).map_err(|e| format!("rusqlite open: {e}"))?;
        let integrity: String = sq
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|e| format!("integrity_check: {e}"))?;
        if integrity != "ok" {
            return Err(format!("integrity_check = {integrity}"));
        }
        let articles: i64 = sq
            .query_row("SELECT COUNT(*) FROM articles", [], |row| row.get(0))
            .map_err(|e| format!("count: {e}"))?;
        drop(sq);
        let _ = std::fs::remove_file(&path);
        if articles != 2 {
            return Err(format!("expected 2 articles after reopen, got {articles}"));
        }
        Ok(())
    })());
}

/// Which PRAGMAs the host's startup sequence relies on are accepted. Records the
/// synchronous-mode support (the host uses NORMAL under rusqlite).
fn check_pragmas(r: &mut Report) {
    let path = temp_db("pragma");
    let db = match open(&path) {
        Ok(db) => db,
        Err(e) => {
            r.check("pragma matrix", Err(e));
            return;
        }
    };
    let conn = db.connect().expect("connect");
    let pragmas = [
        "PRAGMA journal_mode=WAL",
        "PRAGMA foreign_keys=ON",
        "PRAGMA busy_timeout=5000",
        "PRAGMA synchronous=OFF",
        "PRAGMA synchronous=NORMAL",
        "PRAGMA synchronous=FULL",
    ];
    println!("  --   pragma acceptance:");
    for p in pragmas {
        let outcome = exec(&conn, p);
        println!("        {:.<34} {}", p, if outcome.is_ok() { "ok" } else { "UNSUPPORTED" });
    }
    let _ = conn.close();
    let _ = std::fs::remove_file(&path);
    // The only hard requirement: the host must be able to open WAL + set a busy
    // timeout. synchronous-mode choice is a documented durability/latency trade.
    r.check("required pragmas (WAL + busy_timeout) accepted", (|| {
        let path = temp_db("pragma2");
        let db = open(&path)?;
        let conn = db.connect().map_err(|e| format!("{e:?}"))?;
        exec(&conn, "PRAGMA journal_mode=WAL")?;
        exec(&conn, "PRAGMA busy_timeout=5000")?;
        let _ = conn.close();
        let _ = std::fs::remove_file(&path);
        Ok(())
    })());
}

/// Constraint-error classification: UNIQUE and PRIMARY KEY violations surface as
/// distinguishable `Constraint` errors (the host's classify_db_error feeds
/// conformance-exercised 422s).
fn check_constraint_errors(r: &mut Report) {
    r.check("UNIQUE vs PRIMARY KEY violations are classifiable", (|| {
        let db = open(":memory:")?;
        let conn = db.connect().map_err(|e| format!("{e:?}"))?;
        seed(&conn)?;
        // Duplicate UNIQUE email (users.email is UNIQUE).
        let dup_unique = exec(&conn, "INSERT INTO users (id, email, username, password_hash) VALUES (99, 'alice@x.com', 'alice2', 'x')");
        // Duplicate PRIMARY KEY id.
        let dup_pk = exec(&conn, "INSERT INTO users (id, email, username, password_hash) VALUES (1, 'new@x.com', 'newname', 'x')");
        let u = dup_unique.err().ok_or("duplicate email did not fail")?;
        let p = dup_pk.err().ok_or("duplicate PK did not fail")?;
        println!("        UNIQUE  err: {u}");
        println!("        PK      err: {p}");
        let ul = u.to_lowercase();
        let pl = p.to_lowercase();
        if !(ul.contains("email") || ul.contains("unique")) {
            return Err(format!("UNIQUE violation message not classifiable: {u}"));
        }
        if !(pl.contains("id") || pl.contains("primary") || pl.contains("unique")) {
            return Err(format!("PK violation message not classifiable: {p}"));
        }
        Ok(())
    })());
}

/// article-by-slug microbench: turso vs rusqlite, both in-memory, same seed.
/// Gate: turso must not exceed 1.5x rusqlite's per-query time.
const POINT_LOOKUP: &str =
    "SELECT c.id, c.author_id FROM comments c WHERE c.id = :comment_id AND c.article_id = :article_id";

fn check_microbench(r: &mut Report) {
    // A debug build compares optimized-C rusqlite against unoptimized Rust turso
    // and inflates the ratio ~3x — the original B0 mistake. Refuse to report.
    if cfg!(debug_assertions) {
        println!("  --   SKIPPED: run with --release (debug inflates the turso ratio ~3x)");
        r.check("microbench (release build)", Err("built without --release".into()));
        return;
    }
    const N: u32 = 2000;
    // Hot @sql json query.
    let turso_ns = bench_turso(N);
    let rusqlite_ns = bench_rusqlite(N);
    let ratio = turso_ns / rusqlite_ns;
    println!(
        "  --   article-by-slug (@sql json): turso {:.1}µs/q  rusqlite {:.1}µs/q  ratio {ratio:.2}x",
        turso_ns / 1000.0,
        rusqlite_ns / 1000.0,
    );
    // Trivial indexed point lookup — characterizes fixed per-query overhead vs
    // json/subquery execution cost.
    let (t_pt, r_pt) = bench_point_lookup(N);
    let pt_ratio = t_pt / r_pt;
    println!(
        "  --   point lookup (typed):        turso {:.1}µs/q  rusqlite {:.1}µs/q  ratio {pt_ratio:.2}x",
        t_pt / 1000.0,
        r_pt / 1000.0,
    );
    r.check("microbench within 1.5x rusqlite", if ratio <= 1.5 {
        Ok(())
    } else {
        Err(format!("turso is {ratio:.2}x rusqlite on the hot @sql json query (> 1.5x gate); {pt_ratio:.2}x on a point lookup"))
    });
}

fn bench_point_lookup(n: u32) -> (f64, f64) {
    // turso
    let db = open(":memory:").expect("open");
    let conn = db.connect().expect("connect");
    seed(&conn).expect("seed");
    let mut stmt = conn.prepare_single(POINT_LOOKUP).expect("prepare");
    let mut trun = || {
        stmt.reset().expect("reset");
        let p = stmt.named_position(":comment_id").expect("pos");
        stmt.bind_positional(p, Value::from_i64(100)).expect("bind");
        let p = stmt.named_position(":article_id").expect("pos");
        stmt.bind_positional(p, Value::from_i64(10)).expect("bind");
        if let TursoStatusCode::Row = stmt.step(None).expect("step") {
            std::hint::black_box(stmt.row_value(0).expect("v").as_int());
        }
    };
    for _ in 0..50 {
        trun();
    }
    let start = Instant::now();
    for _ in 0..n {
        trun();
    }
    let t = start.elapsed().as_nanos() as f64 / n as f64;
    drop(stmt);

    // rusqlite
    let rconn = rusqlite::Connection::open_in_memory().expect("open");
    for sql in SCHEMA {
        rconn.execute(sql, []).expect("schema");
    }
    rconn.execute_batch(
        "INSERT INTO users (id,email,username,password_hash) VALUES (1,'a@x.com','a','x');
         INSERT INTO articles (id,slug,title,description,body,author_id,created_at,updated_at) VALUES (10,'s','t','d','b',1,'t','t');
         INSERT INTO comments (id,article_id,author_id,body,created_at,updated_at) VALUES (100,10,1,'x','t','t');",
    ).expect("seed");
    let mut rstmt = rconn.prepare(POINT_LOOKUP).expect("prepare");
    let mut rrun = || {
        let v: i64 = rstmt
            .query_row(rusqlite::named_params! { ":comment_id": 100i64, ":article_id": 10i64 }, |row| row.get(0))
            .expect("q");
        std::hint::black_box(v);
    };
    for _ in 0..50 {
        rrun();
    }
    let start = Instant::now();
    for _ in 0..n {
        rrun();
    }
    let rq = start.elapsed().as_nanos() as f64 / n as f64;
    (t, rq)
}

fn bench_turso(n: u32) -> f64 {
    let db = open(":memory:").expect("turso open");
    let conn = db.connect().expect("connect");
    seed(&conn).expect("seed");
    let sql = article_json_by_slug();
    // Prepare ONCE and reuse (reset + rebind) per iteration, matching rusqlite's
    // prepared-statement reuse — an engine-to-engine execution comparison.
    let mut stmt = conn.prepare_single(&sql).expect("prepare");
    let mut run = || {
        stmt.reset().expect("reset");
        let p = stmt.named_position(":slug").expect("pos slug");
        stmt.bind_positional(p, Value::build_text("article-a")).expect("bind slug");
        let p = stmt.named_position(":viewer").expect("pos viewer");
        stmt.bind_positional(p, Value::from_i64(3)).expect("bind viewer");
        match stmt.step(None).expect("step") {
            TursoStatusCode::Row => {
                let doc = stmt.row_value(0).expect("row").to_text().map(str::to_owned);
                std::hint::black_box(doc);
            }
            other => panic!("expected a row, got {other:?}"),
        }
    };
    for _ in 0..50 {
        run();
    }
    let start = Instant::now();
    for _ in 0..n {
        run();
    }
    start.elapsed().as_nanos() as f64 / n as f64
}

fn bench_rusqlite(n: u32) -> f64 {
    let conn = rusqlite::Connection::open_in_memory().expect("rusqlite open");
    for sql in SCHEMA {
        conn.execute(sql, []).expect("schema");
    }
    // Same seed as the turso path (inline to keep the two independent).
    conn.execute_batch(
        "INSERT INTO users (id,email,username,password_hash) VALUES (1,'alice@x.com','alice','x'),(2,'bob@x.com','bob','x'),(3,'viewer@x.com','viewer','x');
         INSERT INTO articles (id,slug,title,description,body,author_id,created_at,updated_at) VALUES (10,'article-a','T10','D10','Body 10',1,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z'),(11,'article-b','T11','D11','Body 11',2,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');
         INSERT INTO article_tags (article_id,tag) VALUES (10,'zeta'),(10,'alpha'),(11,'beta');
         INSERT INTO favorites (user_id,article_id) VALUES (3,10);
         INSERT INTO follows (follower_id,followed_id) VALUES (3,1);",
    ).expect("seed");
    let sql = article_json_by_slug();
    let mut stmt = conn.prepare(&sql).expect("prepare");
    let run = |stmt: &mut rusqlite::Statement| {
        let doc: String = stmt
            .query_row(rusqlite::named_params! { ":slug": "article-a", ":viewer": 3i64 }, |row| row.get(0))
            .expect("q");
        std::hint::black_box(doc);
    };
    for _ in 0..50 {
        run(&mut stmt);
    }
    let start = Instant::now();
    for _ in 0..n {
        run(&mut stmt);
    }
    start.elapsed().as_nanos() as f64 / n as f64
}

/// B4 measurement: how much does rsapi's `row_value().to_owned()` copy cost,
/// relative to reading the cell borrowed? Reads the big article-by-slug JSON
/// document cell N times each way on a turso_core statement (borrowed
/// ValueRef vs owned clone). The delta bounds the single-copy win available by
/// bypassing rsapi's always-owned row_value.
fn measure_b4_copy_cost() {
    use std::num::NonZero;
    use turso_core::Value as CValue;

    let db = open(":memory:").expect("open");
    let conn = db.connect().expect("connect");
    seed(&conn).expect("seed");

    let core_db = db.db_core().expect("db_core");
    let core_conn = turso_core::Database::connect(&core_db).expect("core connect");
    let mut stmt = turso_core::Connection::prepare(&core_conn, article_json_by_slug()).expect("prepare");
    // Param order by first appearance: :viewer (in the summary) then :slug.
    stmt.bind_at(NonZero::new(1).unwrap(), CValue::from_i64(-1)).expect("bind viewer");
    stmt.bind_at(NonZero::new(2).unwrap(), CValue::build_text("article-a")).expect("bind slug");

    // Step to the single row (in-memory: no IO expected).
    loop {
        match stmt.step().expect("step") {
            turso_core::StepResult::Row => break,
            turso_core::StepResult::IO => core_db.io.step().expect("io"),
            other => panic!("expected a row, got {other:?}"),
        }
    }
    let row = stmt.row().expect("row");
    let doc_len = row.get_value(0).to_text().map(|s| s.len()).unwrap_or(0);

    const N: u32 = 100_000;
    // Borrowed: access the cell, no copy.
    for _ in 0..1000 {
        std::hint::black_box(row.get_value(0).to_text().map(|s| s.len()));
    }
    let t0 = Instant::now();
    for _ in 0..N {
        std::hint::black_box(row.get_value(0).to_text().map(|s| s.len()));
    }
    let borrowed_ns = t0.elapsed().as_nanos() as f64 / N as f64;

    // Owned: clone the cell into a turso Value (what rsapi row_value does).
    for _ in 0..1000 {
        std::hint::black_box(row.get_value(0).to_owned());
    }
    let t1 = Instant::now();
    for _ in 0..N {
        std::hint::black_box(row.get_value(0).to_owned());
    }
    let owned_ns = t1.elapsed().as_nanos() as f64 / N as f64;

    let copy_ns = (owned_ns - borrowed_ns).max(0.0);
    println!("  --   article JSON doc: {doc_len} bytes");
    println!("  --   borrowed read:    {borrowed_ns:.1} ns/cell");
    println!("  --   owned (to_owned):  {owned_ns:.1} ns/cell");
    println!("  --   redundant copy:    {copy_ns:.1} ns/cell (~{:.2} µs)", copy_ns / 1000.0);
    println!(
        "        vs ~950µs end-to-end turso byslug p50 → the removable copy is ~{:.2}% of a request",
        copy_ns / 1000.0 / 950.0 * 100.0
    );
}

fn main() {
    println!("turso {} — B0 coverage + perf spike\n", TursoDatabase::version());
    let mut r = Report { failures: Vec::new() };

    println!("[schema + seed]");
    let db = match open(":memory:") {
        Ok(db) => db,
        Err(e) => {
            eprintln!("FATAL: {e}");
            std::process::exit(2);
        }
    };
    let conn = db.connect().expect("connect");
    if let Err(e) = seed(&conn) {
        eprintln!("FATAL seed: {e}");
        std::process::exit(2);
    }
    println!("  ok   schema applied + seeded");

    println!("\n[coverage + flag correctness]");
    check_coverage(&conn, &mut r);
    println!("\n[writer contention]");
    check_contention(&mut r);
    println!("\n[durability + file compat]");
    check_file_compat(&mut r);
    println!("\n[pragma matrix]");
    check_pragmas(&mut r);
    println!("\n[constraint classification]");
    check_constraint_errors(&mut r);
    println!("\n[microbench vs rusqlite]");
    check_microbench(&mut r);

    println!("\n[B4: redundant-copy cost (borrowed vs owned cell read)]");
    measure_b4_copy_cost();

    println!("\n=== GATE VERDICT ===");
    if r.failures.is_empty() {
        println!("GO — all coverage + operational checks passed.");
    } else {
        println!("NO-GO — {} failure(s):", r.failures.len());
        for f in &r.failures {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
}
