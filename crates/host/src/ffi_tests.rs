//! FFI/layout unit tests: exercise the generated glue helpers under a
//! counting allocator so leaks and double-frees fail loudly.
//!
//! Layout itself is enforced at compile time by the generated size/alignment
//! const assertions in roc_platform_abi.rs.

use std::sync::atomic::Ordering;

use crate::abi;
use crate::alloc_counts;
use crate::types::RocKeyValue;

/// What this thread allocated, and how many of those allocations are still
/// live, since `Counters::start`.
struct Counters {
    allocs: isize,
    live: isize,
}

impl Counters {
    fn start() -> Self {
        let (allocs, live) = alloc_counts::now();
        Self { allocs, live }
    }

    fn allocs(&self) -> isize {
        alloc_counts::now().0 - self.allocs
    }

    fn live(&self) -> isize {
        alloc_counts::now().1 - self.live
    }
}

const BIG_TEXT: &str = "a string long enough to defeat the small-string optimization";

#[test]
fn big_string_round_trip_frees_its_allocation() {
    let counters = Counters::start();
    let value = abi::RocStr::from_str(BIG_TEXT);
    assert_eq!(value.as_str(), BIG_TEXT);
    drop(value);
    assert_eq!(counters.live(), 0, "leaked allocations");
    assert!(counters.allocs() > 0, "expected a heap string");
}

#[test]
fn small_string_never_touches_the_allocator() {
    let counters = Counters::start();
    let value = abi::RocStr::from_str("hi");
    assert_eq!(value.as_slice(), b"hi");
    drop(value);
    assert_eq!(counters.allocs(), 0);
}

#[test]
fn a_clone_keeps_the_allocation_until_both_drop() {
    let counters = Counters::start();
    let value = abi::RocStr::from_str(BIG_TEXT);
    let clone = value.clone();
    drop(value);
    assert_eq!(counters.live(), 1, "allocation freed while a reference was still live");
    drop(clone);
    assert_eq!(counters.live(), 0);
}

#[test]
fn byte_list_round_trip_frees_its_allocation() {
    let counters = Counters::start();
    let bytes: Vec<u8> = (0..255).collect();
    let list = abi::RocList::from_slice(&bytes);
    assert_eq!(list.as_slice(), bytes.as_slice());
    drop(list);
    assert_eq!(counters.live(), 0);
}

#[test]
fn key_value_list_frees_its_elements() {
    let counters = Counters::start();
    let list: abi::RocList<RocKeyValue> = (0..3)
        .map(|_| RocKeyValue { name: abi::RocStr::from_str(BIG_TEXT), value: abi::RocStr::from_str(BIG_TEXT) })
        .collect();
    assert_eq!(list.len(), 3);
    drop(list);
    assert_eq!(counters.live(), 0, "leaked element strings");
}

#[test]
fn sql_value_text_payload_is_freed_on_drop() {
    let counters = Counters::start();
    let value = abi::HostSqlValue::text(abi::RocStr::from_str(BIG_TEXT));
    drop(value);
    assert_eq!(counters.live(), 0);
}

#[test]
fn empty_list_allocates_nothing() {
    let counters = Counters::start();
    let list = abi::RocList::<u8>::from_slice(&[]);
    assert!(list.is_empty());
    drop(list);
    assert_eq!(counters.allocs(), 0);
}

// ---------------------------------------------------------------------------
// Borrowed slices (borrow::borrowed_str) — the zero-copy host-owned-buffer trick.
// A seamless slice over the shared static (rc==0) backing must read back the
// borrowed bytes, allocate nothing, and be a no-op under clone/drop so the
// borrowed buffer is never freed.
// ---------------------------------------------------------------------------

const BORROWED_TEXT: &[u8] = b"borrowed bytes owned by the host, not by Roc";

#[test]
fn borrowed_slice_reads_back_without_allocating() {
    let counters = Counters::start();
    let s = unsafe { crate::borrow::borrowed_str(BORROWED_TEXT.as_ptr(), BORROWED_TEXT.len()) };
    assert!(!s.is_small_str(), "a borrow must be a big string, never SSO");
    assert!(s.is_seamless_slice(), "a borrow must be tagged a seamless slice");
    assert_eq!(s.len(), BORROWED_TEXT.len());
    assert_eq!(s.as_slice(), BORROWED_TEXT);
    // bytes point straight at the host buffer — no copy was made.
    assert_eq!(s.as_u8_ptr(), BORROWED_TEXT.as_ptr());
    assert_eq!(counters.allocs(), 0, "a borrow must not allocate");
}

#[test]
fn borrowed_slice_is_immortal_under_refcounting() {
    let counters = Counters::start();
    let s = unsafe { crate::borrow::borrowed_str(BORROWED_TEXT.as_ptr(), BORROWED_TEXT.len()) };
    // rc==0 (static): clone and drop both read the count and do nothing.
    assert!(s.is_unique(), "static data reports unique");
    let clones: Vec<abi::RocStr> = (0..5).map(|_| s.clone()).collect();
    drop(clones);
    assert_eq!(counters.live(), 0, "a borrow must never free anything");
    assert_eq!(counters.allocs(), 0);
    // The borrowed buffer is untouched and still readable.
    assert_eq!(s.as_slice(), BORROWED_TEXT);
}

#[test]
fn borrowed_short_cell_is_still_a_big_string() {
    // A cell shorter than the SSO threshold must NOT become a small string:
    // we always hand Roc the seamless-slice form so `bytes` keeps pointing at
    // the host buffer. (The list serializer relies on this for short cells.)
    let short = b"vue";
    let s = unsafe { crate::borrow::borrowed_str(short.as_ptr(), short.len()) };
    assert!(!s.is_small_str());
    assert!(s.is_seamless_slice());
    assert_eq!(s.as_slice(), short);
    assert_eq!(s.as_u8_ptr(), short.as_ptr());
}

#[test]
fn borrowed_empty_cell_is_a_small_empty_string() {
    let s = unsafe { crate::borrow::borrowed_str(BORROWED_TEXT.as_ptr(), 0) };
    assert!(s.is_empty());
    assert!(s.is_small_str(), "an empty borrow is the small empty string");
}

#[test]
fn borrowed_str_row_spine_frees_only_itself() {
    // A List(Str) of borrowed cells: the SPINE is a normal refcounted list
    // allocation Roc owns and frees; its elements are immortal borrows. Dropping
    // the list must free exactly one allocation (the spine) and leave every
    // borrowed buffer alive.
    let counters = Counters::start();
    let cells: [&[u8]; 3] = [b"react", b"angular", BORROWED_TEXT];
    let list: abi::RocList<abi::RocStr> = cells
        .iter()
        .map(|cell| unsafe { crate::borrow::borrowed_str(cell.as_ptr(), cell.len()) })
        .collect();
    assert_eq!(counters.live(), 1, "only the spine is allocated");
    drop(list);
    assert_eq!(counters.live(), 0, "the spine must be freed");
    assert_eq!(counters.allocs(), 1, "exactly one allocation total");
    // Borrowed buffers survive the list teardown.
    assert_eq!(cells[2], BORROWED_TEXT);
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

/// Build `HostSqlBeginArgs` from owned host values, which the cursor fns take
/// over.
fn begin_args(ctx: u64, db: &str, sql: &str) -> abi::HostSqlBeginArgs {
    use crate::roc_call::roc_str;
    abi::HostSqlBeginArgs {
        ctx,
        db: roc_str(db),
        params: abi::RocList::empty(),
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
    match result.into_view() {
        abi::HostSqlBeginResultOwnedView::Ok(handle) => handle,
        abi::HostSqlBeginResultOwnedView::Err(message) => panic!("sql_begin! failed: {}", message.as_str()),
    }
}

/// The `Ok` payload of a `sql_step!` result.
fn has_row(result: abi::HostSqlStepResult) -> bool {
    match result.into_view() {
        abi::HostSqlStepResultOwnedView::Ok(has_row) => has_row,
        abi::HostSqlStepResultOwnedView::Err(message) => panic!("sql_step! failed: {}", message.as_str()),
    }
}

#[test]
fn cursor_round_trip_reads_typed_cells() {
    use crate::cursor::*;
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
    drop(cols);
    assert_eq!(names, vec!["id", "name", "bio"]);

    // First (only) row.
    assert!(has_row(roc_sql_step(handle)), "expected a row");

    assert_eq!(roc_sql_col_i64(handle, 0), 7);
    assert_eq!(roc_sql_col_type(handle, 0), abi::HostColType::Integer);

    let name = roc_sql_col_str(handle, 1);
    assert_eq!(name.as_slice(), b"jake");
    drop(name);
    assert_eq!(roc_sql_col_type(handle, 1), abi::HostColType::Text);

    // NULL bio: type probe reports Null (the corruption backstop).
    assert_eq!(roc_sql_col_type(handle, 2), abi::HostColType::Null);

    // End of results.
    assert!(!has_row(roc_sql_step(handle)), "expected end of rows");

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
    let text = match result.into_view() {
        abi::HostSqlBeginResultOwnedView::Err(message) => message.as_str().to_owned(),
        abi::HostSqlBeginResultOwnedView::Ok(_) => panic!("expected sql_begin! to fail"),
    };
    assert!(text.contains("missing_table") || text.contains("no such"), "got: {text}");
    // A failed begin registers no cursor under its ctx.
    assert_eq!(sweep_ctx_cursors(7_400_004), 0, "a failed begin registers no cursor");
}

#[test]
fn password_hash_then_verify_round_trips() {
    use crate::effects::{password_hash, password_verify};
    use crate::roc_call::roc_str;

    let hash = password_hash(roc_str("s3cret"));
    let phc = String::from_utf8_lossy(hash.as_slice()).into_owned();
    drop(hash);
    assert!(phc.starts_with("$argon2id$"), "expected a PHC string, got {phc}");
    assert!(password_verify(roc_str("s3cret"), roc_str(&phc)));
    assert!(!password_verify(roc_str("wrong"), roc_str(&phc)));
    assert!(!password_verify(roc_str("s3cret"), roc_str("not-a-phc-hash")));
}
