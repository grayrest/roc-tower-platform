//! FFI/layout unit tests: exercise the generated glue helpers under a
//! counting allocator so leaks and double-frees fail loudly.
//!
//! Layout itself is enforced at compile time by the generated size/alignment
//! const assertions in roc_platform_abi.rs.

use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use crate::abi;
use crate::types::RocKeyValue;

/// Process-wide allocation counter for the microbench. Roc's default allocator
/// routes through `std::alloc`, so this counts BOTH the Roc-side marshalling
/// (RocStr/RocList) and Rust-side (Vec/String) allocations of each SQL path.
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;
unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new: usize) -> *mut u8 {
        unsafe { std::alloc::System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[derive(Default)]
struct Counters {
    live: AtomicIsize,
    allocs: AtomicIsize,
}

extern "C" fn counting_alloc(
    host: *mut abi::RocHost,
    length: usize,
    alignment: usize,
) -> *mut c_void {
    let counters = unsafe { &*((*host).env as *const Counters) };
    counters.live.fetch_add(1, Ordering::SeqCst);
    counters.allocs.fetch_add(1, Ordering::SeqCst);
    abi::DefaultAllocators::roc_alloc(host, length, alignment)
}

extern "C" fn counting_dealloc(host: *mut abi::RocHost, ptr: *mut c_void, alignment: usize) {
    let counters = unsafe { &*((*host).env as *const Counters) };
    counters.live.fetch_sub(1, Ordering::SeqCst);
    abi::DefaultAllocators::roc_dealloc(host, ptr, alignment)
}

extern "C" fn counting_realloc(
    host: *mut abi::RocHost,
    ptr: *mut c_void,
    new_length: usize,
    alignment: usize,
) -> *mut c_void {
    // Net-zero for live count: one allocation replaces another.
    abi::DefaultAllocators::roc_realloc(host, ptr, new_length, alignment)
}

fn counting_host(counters: &Counters) -> abi::RocHost {
    let mut host = abi::make_roc_host(counters as *const Counters as *mut c_void);
    host.roc_alloc = counting_alloc;
    host.roc_dealloc = counting_dealloc;
    host.roc_realloc = counting_realloc;
    host
}

const BIG_TEXT: &[u8] = b"a string long enough to defeat the small-string optimization";

#[test]
fn big_string_round_trip_frees_its_allocation() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    let value = abi::RocStr::from_slice(BIG_TEXT, &host);
    assert_eq!(value.as_slice(), BIG_TEXT);
    unsafe { value.decref(&host) };
    assert_eq!(counters.live.load(Ordering::SeqCst), 0, "leaked allocations");
    assert!(counters.allocs.load(Ordering::SeqCst) > 0, "expected a heap string");
}

#[test]
fn small_string_never_touches_the_allocator() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    let value = abi::RocStr::from_slice(b"hi", &host);
    assert_eq!(value.as_slice(), b"hi");
    unsafe { value.decref(&host) };
    assert_eq!(counters.allocs.load(Ordering::SeqCst), 0);
}

#[test]
fn incref_requires_two_decrefs_to_free() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    let value = abi::RocStr::from_slice(BIG_TEXT, &host);
    unsafe {
        value.incref(1);
        value.decref(&host);
        assert_eq!(
            counters.live.load(Ordering::SeqCst),
            1,
            "allocation freed while a reference was still live"
        );
        value.decref(&host);
    }
    assert_eq!(counters.live.load(Ordering::SeqCst), 0);
}

#[test]
fn byte_list_round_trip_frees_its_allocation() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    let bytes: Vec<u8> = (0..255).collect();
    let list = unsafe { abi::RocListWith::<u8, false>::from_slice(&bytes, &host) };
    assert_eq!(list.as_slice(), bytes.as_slice());
    unsafe { list.decref(&host) };
    assert_eq!(counters.live.load(Ordering::SeqCst), 0);
}

#[test]
fn key_value_list_recursively_frees_elements() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    unsafe {
        let list = abi::RocList::<RocKeyValue>::allocate(3, &host);
        for index in 0..3 {
            list.elements.add(index).write(RocKeyValue {
                name: abi::RocStr::from_slice(BIG_TEXT, &host),
                value: abi::RocStr::from_slice(BIG_TEXT, &host),
            });
        }
        // Same teardown pattern the generated recursive helpers use.
        if list.has_one_ref() {
            for item in list.allocation_items() {
                item.decref(&host);
            }
        }
        list.decref(&host);
    }
    assert_eq!(counters.live.load(Ordering::SeqCst), 0, "leaked element strings");
}

#[test]
fn sql_value_text_payload_is_freed_by_generated_decref() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    let value = abi::HostSqlValue {
        tag: abi::HostSqlValueTag::Text,
        payload: abi::HostSqlValuePayload {
            text: core::mem::ManuallyDrop::new(abi::RocStr::from_slice(BIG_TEXT, &host)),
        },
    };
    unsafe { value.decref(&host) };
    assert_eq!(counters.live.load(Ordering::SeqCst), 0);
}

#[test]
fn empty_list_allocates_nothing() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    let list = unsafe { abi::RocListWith::<u8, false>::from_slice(&[], &host) };
    assert!(list.is_empty());
    unsafe { list.decref(&host) };
    assert_eq!(counters.allocs.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Cursor FFI (Host.sql_begin! … sql_finish!).
// ---------------------------------------------------------------------------

use std::sync::atomic::AtomicU64;

/// A unique temp db path per test, so pools built for one test don't bleed
/// into another.
fn temp_db(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "tower-cursor-{tag}-{}-{n}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().into_owned()
}

/// Build `HostSqlBeginArgs` from owned host values (allocated on the global
/// host, which the cursor fns decref against).
fn begin_args(ctx: u64, db: &str, sql: &str) -> abi::HostSqlBeginArgs {
    use crate::roc_call::roc_str;
    use crate::roc_host;
    let params = unsafe { abi::RocList::<abi::HostSqlValue>::allocate(0, roc_host()) };
    abi::HostSqlBeginArgs {
        ctx,
        db: roc_str(db),
        params,
        sql: roc_str(sql),
    }
}

fn seed(db: &str) {
    let conn = rusqlite::Connection::open(db).expect("open temp db");
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, bio TEXT);
         INSERT INTO t (id, name, bio) VALUES (7, 'jake', NULL);",
    )
    .expect("seed");
}

fn handle_of(result: abi::HostSqlBeginResult) -> u64 {
    assert_eq!(result.tag, abi::HostSqlBeginResultTag::Ok, "sql_begin! failed");
    result.payload_ok()
}

#[test]
fn cursor_round_trip_reads_typed_cells() {
    use crate::cursor::*;
    use crate::roc_host;
    let db = temp_db("roundtrip");
    seed(&db);

    // A unique ctx so assertions are independent of other tests running in
    // parallel against the shared cursor registry.
    let ctx = 7_100_001;
    let handle = handle_of(roc_sql_begin(begin_args(ctx, &db, "SELECT id, name, bio FROM t WHERE id = 7")));

    // Column names cached at begin.
    let cols = roc_sql_columns(handle);
    let names: Vec<String> = cols
        .as_slice()
        .iter()
        .map(|s| String::from_utf8_lossy(s.as_slice()).into_owned())
        .collect();
    unsafe {
        for s in cols.allocation_items() {
            s.decref(roc_host());
        }
        cols.decref(roc_host());
    }
    assert_eq!(names, vec!["id", "name", "bio"]);

    // First (only) row.
    let step = roc_sql_step(handle);
    assert_eq!(step.tag, abi::HostSqlStepResultTag::Ok);
    assert!(step.payload_ok(), "expected a row");

    assert_eq!(roc_sql_col_i64(handle, 0), 7);
    assert_eq!(roc_sql_col_type(handle, 0), abi::HostColType::Integer);

    let name = roc_sql_col_str(handle, 1);
    assert_eq!(name.as_slice(), b"jake");
    unsafe { name.decref(roc_host()) };
    assert_eq!(roc_sql_col_type(handle, 1), abi::HostColType::Text);

    // NULL bio: type probe reports Null (the corruption backstop).
    assert_eq!(roc_sql_col_type(handle, 2), abi::HostColType::Null);

    // End of results.
    let done = roc_sql_step(handle);
    assert!(!done.payload_ok(), "expected end of rows");

    roc_sql_finish(handle);
    // finish removed it: a ctx sweep finds nothing left to finalize.
    assert_eq!(sweep_ctx_cursors(ctx), 0, "finish must remove the cursor");
}

#[test]
fn cursor_left_open_is_swept_by_ctx() {
    use crate::cursor::*;
    let db = temp_db("leak");
    seed(&db);

    // Open two cursors under a unique ctx and never finish them.
    let ctx = 7_200_002;
    let _h1 = handle_of(roc_sql_begin(begin_args(ctx, &db, "SELECT id FROM t")));
    let _h2 = handle_of(roc_sql_begin(begin_args(ctx, &db, "SELECT name FROM t")));

    // The ctx teardown sweep (what the router's reaper runs) finalizes exactly
    // this ctx's orphaned cursors, regardless of other in-flight tests.
    assert_eq!(sweep_ctx_cursors(ctx), 2, "sweep must finalize the ctx's cursors");
    assert_eq!(sweep_ctx_cursors(ctx), 0, "a second sweep finds none left");
}

#[test]
fn cursor_finish_is_idempotent() {
    use crate::cursor::*;
    let db = temp_db("idempotent");
    seed(&db);

    let ctx = 7_300_003;
    let handle = handle_of(roc_sql_begin(begin_args(ctx, &db, "SELECT id FROM t")));
    roc_sql_finish(handle);
    // Second finish is a no-op, not a double-free: the handle is no longer in
    // the registry, so the freed pointer is never reconstituted.
    roc_sql_finish(handle);
    // A handle that was never issued is likewise a safe no-op (not in the
    // registry). step/getters trust their handle per the non-escaping contract,
    // so they are not exercised with a bogus handle here.
    roc_sql_finish(0);
    roc_sql_finish(123_456);
    // Nothing left under this ctx.
    assert_eq!(sweep_ctx_cursors(ctx), 0);
}

#[test]
fn cursor_begin_reports_sql_errors() {
    use crate::cursor::*;
    let db = temp_db("badsql");
    seed(&db);
    let result = roc_sql_begin(begin_args(7_400_004, &db, "SELECT nope FROM missing_table"));
    assert_eq!(result.tag, abi::HostSqlBeginResultTag::Err);
    let message = result.payload_err();
    let text = String::from_utf8_lossy(message.as_slice()).into_owned();
    unsafe { message.decref(crate::roc_host()) };
    assert!(text.contains("missing_table") || text.contains("no such"), "got: {text}");
    // A failed begin registers no cursor under its ctx.
    assert_eq!(sweep_ctx_cursors(7_400_004), 0, "a failed begin registers no cursor");
}

// ---------------------------------------------------------------------------
// Microbench gate (plan decision 7): cursor loop vs sqlite_exec! on
// article_select. Run with:
//   cargo test -p tower-platform-host microbench -- --ignored --nocapture
// It is #[ignore]d so it never runs in the normal gate (timing is noisy and it
// is a one-time go/no-go, recorded in notes/).
// ---------------------------------------------------------------------------

const BENCH_ARTICLE_SELECT: &str = "\
SELECT a.id, a.author_id, a.slug, a.title, a.description, a.body, a.created_at, a.updated_at,
       u.username, u.bio, u.image,
       (SELECT COUNT(*) FROM favorites f WHERE f.article_id = a.id) AS favorites_count,
       EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = ?) AS favorited,
       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = ? AND fo.followed_id = a.author_id) AS following,
       (SELECT GROUP_CONCAT(tag) FROM (SELECT at2.tag FROM article_tags at2 WHERE at2.article_id = a.id ORDER BY at2.tag)) AS tag_list
FROM articles a JOIN users u ON u.id = a.author_id ORDER BY a.id DESC LIMIT 20";

fn seed_articles(db: &str) {
    let conn = rusqlite::Connection::open(db).expect("open");
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, bio TEXT, image TEXT);
         CREATE TABLE follows (follower_id INTEGER NOT NULL, followed_id INTEGER NOT NULL, PRIMARY KEY (follower_id, followed_id));
         CREATE TABLE articles (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, author_id INTEGER NOT NULL REFERENCES users(id), created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
         CREATE TABLE article_tags (article_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY (article_id, tag));
         CREATE TABLE favorites (user_id INTEGER NOT NULL, article_id INTEGER NOT NULL, PRIMARY KEY (user_id, article_id));
         CREATE INDEX idx_articles_author ON articles(author_id);
         CREATE INDEX idx_article_tags_tag ON article_tags(tag);
         CREATE INDEX idx_favorites_article ON favorites(article_id, user_id);",
    ).expect("schema");
    for u in 1..=3 {
        conn.execute(
            "INSERT INTO users (id, email, username, password_hash, bio, image) VALUES (?1, ?2, ?3, 'h', ?4, NULL)",
            rusqlite::params![u, format!("u{u}@x"), format!("user{u}"), if u == 2 { None } else { Some("bio") }],
        ).unwrap();
    }
    conn.execute("INSERT INTO follows VALUES (1, 2)", []).unwrap();
    for a in 1..=20 {
        let author = (a % 3) + 1;
        conn.execute(
            "INSERT INTO articles (id, slug, title, description, body, author_id, created_at, updated_at) VALUES (?1, ?2, 'T', 'D', 'B', ?3, '2026-01-01', '2026-01-01')",
            rusqlite::params![a, format!("slug-{a}"), author],
        ).unwrap();
        conn.execute("INSERT INTO article_tags VALUES (?1, 'dragons')", [a]).unwrap();
        conn.execute("INSERT INTO article_tags VALUES (?1, 'training')", [a]).unwrap();
        if a % 2 == 0 {
            conn.execute("INSERT INTO favorites VALUES (1, ?1)", [a]).unwrap();
        }
    }
}

fn int_val(n: i64) -> abi::HostSqlValue {
    abi::HostSqlValue {
        tag: abi::HostSqlValueTag::Integer,
        payload: abi::HostSqlValuePayload {
            integer: core::mem::ManuallyDrop::new(n),
        },
    }
}

fn viewer_params() -> abi::RocList<abi::HostSqlValue> {
    use crate::roc_host;
    unsafe {
        let list = abi::RocList::<abi::HostSqlValue>::allocate(2, roc_host());
        list.elements.add(0).write(int_val(1));
        list.elements.add(1).write(int_val(1));
        list
    }
}

/// Free a `sqlite_exec!` Ok result (mirrors build_sqlite_ok's structure).
unsafe fn free_exec_ok(ok: crate::types::RocSqliteResultOk) {
    use crate::roc_host;
    let host = roc_host();
    unsafe {
        if ok.columns.has_one_ref() {
            for s in ok.columns.allocation_items() {
                s.decref(host);
            }
        }
        ok.columns.decref(host);
        if ok.rows.has_one_ref() {
            for inner in ok.rows.allocation_items() {
                if inner.has_one_ref() {
                    for cell in inner.allocation_items() {
                        cell.decref(host);
                    }
                }
                inner.decref(host);
            }
        }
        ok.rows.decref(host);
    }
}

#[test]
#[ignore = "one-time perf go/no-go; run with --ignored --nocapture"]
fn microbench_cursor_vs_exec() {
    use crate::cursor::*;
    use crate::effects::roc_sqlite_exec;
    use crate::roc_call::roc_str;
    use crate::roc_host;
    use std::time::Instant;

    let db = temp_db("microbench");
    seed_articles(&db);
    let iters = 3000u32;

    // --- old path: sqlite_exec! (materializes List(List(SqlValue))) ---
    let old_exec = || {
        let args = abi::HostSqliteExecArgs {
            db: roc_str(&db),
            params: viewer_params(),
            sql: roc_str(BENCH_ARTICLE_SELECT),
        };
        let result = roc_sqlite_exec(args);
        assert_eq!(result.tag, abi::HostSqliteExecResultTag::Ok);
        unsafe { free_exec_ok(result.payload_ok()) };
    };
    // --- new path: cursor loop, marshalling every cell the way generated code
    // would (typed getters; nullable text via col_type probe) ---
    let new_cursor = || {
        let args = abi::HostSqlBeginArgs {
            ctx: 0,
            db: roc_str(&db),
            params: viewer_params(),
            sql: roc_str(BENCH_ARTICLE_SELECT),
        };
        let handle = handle_of(roc_sql_begin(args));
        loop {
            let step = roc_sql_step(handle);
            if !step.payload_ok() {
                break;
            }
            // 15 columns: ints at 0,1,11,12,13; text elsewhere; 9,10 nullable.
            let _ = roc_sql_col_i64(handle, 0);
            let _ = roc_sql_col_i64(handle, 1);
            for c in [2u64, 3, 4, 5, 6, 7, 8] {
                unsafe { roc_sql_col_str(handle, c).decref(roc_host()) };
            }
            for c in [9u64, 10, 14] {
                if roc_sql_col_type(handle, c) != abi::HostColType::Null {
                    unsafe { roc_sql_col_str(handle, c).decref(roc_host()) };
                }
            }
            let _ = roc_sql_col_i64(handle, 11);
            let _ = roc_sql_col_i64(handle, 12);
            let _ = roc_sql_col_i64(handle, 13);
        }
        roc_sql_finish(handle);
    };

    // Warm the pool + statement cache for both.
    for _ in 0..50 {
        old_exec();
        new_cursor();
    }

    let a0 = ALLOC_COUNT.load(Ordering::Relaxed);
    let t0 = Instant::now();
    for _ in 0..iters {
        old_exec();
    }
    let old_time = t0.elapsed();
    let old_allocs = ALLOC_COUNT.load(Ordering::Relaxed) - a0;

    let a1 = ALLOC_COUNT.load(Ordering::Relaxed);
    let t1 = Instant::now();
    for _ in 0..iters {
        new_cursor();
    }
    let new_time = t1.elapsed();
    let new_allocs = ALLOC_COUNT.load(Ordering::Relaxed) - a1;

    let old_ns = old_time.as_nanos() / iters as u128;
    let new_ns = new_time.as_nanos() / iters as u128;
    eprintln!("microbench article_select x20 rows, {iters} iters:");
    eprintln!("  sqlite_exec!  {old_ns:>7} ns/query  {:>6} allocs/query", old_allocs / iters as usize);
    eprintln!("  cursor        {new_ns:>7} ns/query  {:>6} allocs/query", new_allocs / iters as usize);
    eprintln!("  time ratio cursor/exec = {:.2}", new_ns as f64 / old_ns as f64);
    eprintln!("  alloc ratio cursor/exec = {:.2}", new_allocs as f64 / old_allocs as f64);

    // GATE: cursor must be at parity on time and allocate fewer.
    assert!(
        new_allocs < old_allocs,
        "cursor should allocate fewer ({new_allocs} vs {old_allocs})"
    );
    assert!(
        new_ns as f64 <= old_ns as f64 * 1.20,
        "cursor time regressed beyond parity: {new_ns} vs {old_ns} ns"
    );
}

#[test]
fn password_hash_then_verify_round_trips() {
    use crate::effects::{password_hash, password_verify};
    use crate::roc_call::roc_str;
    use crate::roc_host;

    let hash = password_hash(roc_str("s3cret"));
    let phc = String::from_utf8_lossy(hash.as_slice()).into_owned();
    unsafe { hash.decref(roc_host()) };
    assert!(phc.starts_with("$argon2id$"), "expected a PHC string, got {phc}");
    assert!(password_verify(roc_str("s3cret"), roc_str(&phc)));
    assert!(!password_verify(roc_str("wrong"), roc_str(&phc)));
    assert!(!password_verify(roc_str("s3cret"), roc_str("not-a-phc-hash")));
}
