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
