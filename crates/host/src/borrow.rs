//! Zero-copy borrowed slices into host-owned buffers.
//!
//! A `RocStr`/`RocList` carries three fields — `bytes`, `capacity_or_alloc_ptr`,
//! `length` — and the refcount lives at `alloc_ptr - word`, where `alloc_ptr` is
//! the *masked* `capacity_or_alloc_ptr` of a seamless slice, NOT `bytes`. So we
//! can point `bytes` straight at a host buffer (e.g. a live SQLite cell) while
//! `alloc_ptr` points at a slot we control. Put a `0` refcount just before that
//! slot and the value is `REFCOUNT_STATIC_DATA`: both this host's helpers and
//! the compiler's generated incref/decref return without touching a count when
//! they read `0` (see `roc/src/lir/immortal_locals.zig`). The borrow is then
//! immortal — Roc reads it with native ops but never frees or mutates it.
//!
//! Because the refcount word is only ever read (never written), a single shared
//! static block backs every borrow across all threads.
//!
//! This module is HAND-WRITTEN and must not move into `roc_platform_abi.rs`,
//! which `just glue` regenerates from `vendor/RustGlue.roc` and would clobber.

use crate::abi;

/// The low bit of `capacity_or_alloc_ptr` marks a seamless slice (mirrors
/// `ROC_SEAMLESS_SLICE_TAG` in the generated glue). Slice allocations are
/// word-aligned, so the low bit is always free to carry the tag.
const ROC_SEAMLESS_SLICE_TAG: usize = 1;

/// Shared, process-global, zero-filled backing for every borrow. A seamless
/// slice whose `alloc_ptr` is `&BORROW_BACKING[1]` reads its refcount at
/// `&BORROW_BACKING[0]` == `0` == `REFCOUNT_STATIC_DATA`. Never written.
static BORROW_BACKING: [usize; 2] = [0, 0];

/// Build a `RocStr` that BORROWS `len` bytes at `ptr` without copying, as a
/// seamless slice over the shared static (rc==0) backing. `bytes` points
/// straight at the host buffer; Roc treats the value as immortal read-only
/// data. An empty borrow is the small empty string.
///
/// # Safety
/// `ptr..ptr+len` must be valid UTF-8 that stays alive and unchanged for as
/// long as ANY Roc reference to the returned string can exist. The returned
/// string MUST NOT outlive the borrowed bytes — a live SQLite cell pointer is
/// invalidated by the next step/reset/finalize on its statement, so a borrowed
/// cell must be fully consumed before the cursor advances.
pub unsafe fn borrowed_str(ptr: *const u8, len: usize) -> abi::RocStr {
    if len == 0 {
        return abi::RocStr::empty();
    }
    debug_assert!(
        core::str::from_utf8(unsafe { core::slice::from_raw_parts(ptr, len) }).is_ok(),
        "borrowed_str requires valid UTF-8",
    );
    let slot = core::ptr::addr_of!(BORROW_BACKING[1]) as usize;
    // SAFETY: a seamless slice over static data whose refcount word reads 0, so
    // no clone or drop ever touches a count; the caller keeps the bytes alive.
    unsafe { abi::RocStr::from_raw_parts(ptr as *mut u8, slot | ROC_SEAMLESS_SLICE_TAG, len) }
}

/// Build a `List(U8)` that BORROWS `len` bytes at `ptr` without copying — the
/// blob counterpart to [`borrowed_str`], a seamless slice over the same shared
/// static (rc==0) backing. Non-refcounted element list. An empty borrow is the
/// empty list.
///
/// # Safety
/// Same contract as [`borrowed_str`]: `ptr..ptr+len` must stay alive and
/// unchanged for as long as any Roc reference to the returned list can exist,
/// and the list must not outlive the borrowed bytes.
pub unsafe fn borrowed_bytes(ptr: *const u8, len: usize) -> abi::RocList<u8> {
    if len == 0 {
        return abi::RocList::empty();
    }
    let slot = core::ptr::addr_of!(BORROW_BACKING[1]) as usize;
    let raw = abi::RocListRaw::<u8, false> {
        elements: ptr as *mut u8,
        length: len,
        capacity_or_alloc_ptr: slot | ROC_SEAMLESS_SLICE_TAG,
    };
    // SAFETY: as for `borrowed_str`.
    unsafe { raw.into_list() }
}
