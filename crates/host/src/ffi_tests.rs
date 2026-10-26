//! FFI/layout unit tests: exercise the generated glue helpers under a
//! counting allocator so leaks and double-frees fail loudly.
//!
//! Layout itself is enforced at compile time by the generated size/alignment
//! const assertions in roc_platform_abi.rs.

use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use crate::abi;
use crate::types::RocKeyValue;

#[derive(Default)]
struct Counters {
    live: AtomicIsize,
    allocs: AtomicIsize,
}

extern "C" fn counting_alloc(
    host: *mut abi::RocHost,
    length: usize,
    alignment: usize,
) -> core::ptr::NonNull<c_void> {
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
) -> core::ptr::NonNull<c_void> {
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
// Borrowed slices (borrow::borrowed_str) — the zero-copy host-owned-buffer trick.
// A seamless slice over the shared static (rc==0) backing must read back the
// borrowed bytes, allocate nothing, and be a no-op under incref/decref so the
// borrowed buffer is never freed.
// ---------------------------------------------------------------------------

const BORROWED_TEXT: &[u8] = b"borrowed bytes owned by the host, not by Roc";

#[test]
fn borrowed_slice_reads_back_without_allocating() {
    let counters = Counters::default();
    let _host = counting_host(&counters);
    let s = unsafe { crate::borrow::borrowed_str(BORROWED_TEXT.as_ptr(), BORROWED_TEXT.len()) };
    assert!(!s.is_small_str(), "a borrow must be a big string, never SSO");
    assert!(s.is_seamless_slice(), "a borrow must be tagged a seamless slice");
    assert_eq!(s.len(), BORROWED_TEXT.len());
    assert_eq!(s.as_slice(), BORROWED_TEXT);
    // bytes point straight at the host buffer — no copy was made.
    assert_eq!(s.as_u8_ptr(), BORROWED_TEXT.as_ptr());
    assert_eq!(counters.allocs.load(Ordering::SeqCst), 0, "a borrow must not allocate");
}

#[test]
fn borrowed_slice_is_immortal_under_refcounting() {
    let counters = Counters::default();
    let host = counting_host(&counters);
    let s = unsafe { crate::borrow::borrowed_str(BORROWED_TEXT.as_ptr(), BORROWED_TEXT.len()) };
    // rc==0 (static): incref and decref both read the count and do nothing.
    assert!(s.is_unique(), "static data reports unique");
    unsafe {
        s.incref(5);
        s.decref(&host);
        s.decref(&host);
    }
    assert_eq!(counters.live.load(Ordering::SeqCst), 0, "a borrow must never free anything");
    assert_eq!(counters.allocs.load(Ordering::SeqCst), 0);
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
    // allocation Roc owns and frees; its elements are immortal borrows. The
    // recursive teardown must free exactly one allocation (the spine) and leave
    // every borrowed buffer alive.
    let counters = Counters::default();
    let host = counting_host(&counters);
    let cells: [&[u8]; 3] = [b"react", b"angular", BORROWED_TEXT];
    let list = unsafe { abi::RocList::<abi::RocStr>::allocate(cells.len(), &host) };
    for (index, cell) in cells.iter().enumerate() {
        unsafe {
            list.elements
                .add(index)
                .write(crate::borrow::borrowed_str(cell.as_ptr(), cell.len()));
        }
    }
    assert_eq!(counters.live.load(Ordering::SeqCst), 1, "only the spine is allocated");
    // Same recursive teardown the generated decref_list_of_str helper performs.
    unsafe { list.release_with::<abi::RocStrRelease>(&host) };
    assert_eq!(counters.live.load(Ordering::SeqCst), 0, "the spine must be freed");
    assert_eq!(counters.allocs.load(Ordering::SeqCst), 1, "exactly one allocation total");
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
    // SAFETY: tag asserted immediately above.
    unsafe { *result.borrow_payload_ok_unchecked() }
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
    // SAFETY: tag asserted immediately above.
    assert!(unsafe { *step.borrow_payload_ok_unchecked() }, "expected a row");

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
    assert_eq!(done.tag, abi::HostSqlStepResultTag::Ok);
    // SAFETY: tag asserted immediately above.
    assert!(!unsafe { *done.borrow_payload_ok_unchecked() }, "expected end of rows");

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
    let mut result = roc_sql_begin(begin_args(7_400_004, &db, "SELECT nope FROM missing_table"));
    assert_eq!(result.tag, abi::HostSqlBeginResultTag::Err);
    // SAFETY: tag asserted immediately above. TAKEN, not borrowed: the
    // message is decref'd below, so this needs the owned value.
    let message = unsafe { result.take_payload_err_unchecked() };
    let text = String::from_utf8_lossy(message.as_slice()).into_owned();
    unsafe { message.decref(crate::roc_host()) };
    assert!(text.contains("missing_table") || text.contains("no such"), "got: {text}");
    // A failed begin registers no cursor under its ctx.
    assert_eq!(sweep_ctx_cursors(7_400_004), 0, "a failed begin registers no cursor");
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
