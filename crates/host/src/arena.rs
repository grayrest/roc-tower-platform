//! Request-scoped bump arena behind the `roc_alloc`/`roc_dealloc`/`roc_realloc`
//! symbols that interpreted Roc code (and the host's own Roc-value helpers) call.
//!
//! # Why
//! Profiling put ~31.7% of list-endpoint host CPU in `malloc`/`free`: every Roc
//! value built while serving a request is individually allocated and, on the
//! way out, individually freed. A whole request's Roc values share one lifetime
//! — they are all dead once the response has been written — so a bump allocator
//! whose backing is released in bulk when the request ends replaces N frees with
//! one, and turns each alloc into a pointer bump.
//!
//! # How a request installs the arena
//! The request lifecycle (see `router.rs`) mints an `Arc<RequestArena>` and
//! installs it in this module's thread-local slot on the blocking thread that
//! runs the Roc handler. While installed, `roc_alloc` bump-allocates from it;
//! `roc_dealloc` of an arena pointer is a no-op (the bytes are reclaimed when the
//! last `Arc` drops). Outside a request (model init, background threads) the slot
//! is empty and every call falls through to the global allocator unchanged.
//!
//! # Header layout and the arena tag
//! We keep the *exact* header layout the generated `DefaultAllocators` uses:
//! `size_storage_bytes = max(alignment, word)` bytes of header, with the total
//! allocation size written into the last word before the returned user pointer.
//! Roc never reads this word (its refcount lives at or after the user pointer),
//! so we steal the two high bits of that size word:
//!   * `TAG_ARENA` marks a pointer as arena-owned, so `dealloc`/`realloc` know
//!     not to hand it to the global allocator. The tag reflects where the bytes
//!     actually live — a cap-boundary allocation that falls through to global is
//!     written with a plain (untagged) header.
//!   * `TAG_FREED` is a debug-only double-free tripwire: an arena `dealloc` is a
//!     no-op, which would otherwise silently absorb the double-free bugs that the
//!     global allocator turns into loud crashes.
//!
//! # Concurrency
//! An `Arc<RequestArena>` is genuinely shared across threads (the blocking
//! handler thread allocates; the ctx-guard reaper and the response body owner
//! hold references and race to perform the final drop). Interior state is
//! therefore behind a `Mutex` so the type is honestly `Sync`; allocation only
//! ever happens on the single installing thread, so the lock is uncontended.

use core::ffi::c_void;
use std::alloc::Layout;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::abi;

const WORD: usize = core::mem::size_of::<usize>();

/// High bit of the size word: this allocation is arena-owned.
const TAG_ARENA: usize = 1 << (usize::BITS - 1);
/// Second-highest bit: debug-only "already freed" marker for arena pointers.
const TAG_FREED: usize = 1 << (usize::BITS - 2);
/// Mask recovering the real total size from a (possibly tagged) size word.
const SIZE_MASK: usize = !(TAG_ARENA | TAG_FREED);

/// Default backing-chunk size. Allocations larger than this get a dedicated
/// right-sized chunk instead.
const CHUNK_SIZE: usize = 64 * 1024;
/// Alignment every standard chunk base is allocated to. Covers every Roc
/// alignment we see (max is the 16-byte erased-callable payload); larger
/// one-off allocations bump the dedicated chunk's alignment to match.
const CHUNK_ALIGN: usize = 64;
/// Per-request bump budget. Past this, allocations fall through to the global
/// allocator (with plain headers) so a pathological request can't pin unbounded
/// memory to the arena's single lifetime.
const ARENA_CAP: usize = 4 * 1024 * 1024;

#[cfg(test)]
pub(crate) static CHUNK_ALLOCS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static CHUNK_FREES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Header bytes for a given alignment — identical to `DefaultAllocators`.
#[inline]
fn header_size(alignment: usize) -> usize {
    alignment.max(WORD)
}

#[inline]
unsafe fn read_size_word(user_ptr: *const u8) -> usize {
    unsafe { *(user_ptr.sub(WORD) as *const usize) }
}

#[inline]
unsafe fn write_size_word(user_ptr: *mut u8, value: usize) {
    unsafe { *(user_ptr.sub(WORD) as *mut usize) = value };
}

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    (value + (align - 1)) & !(align - 1)
}

/// One backing allocation owned by the arena; freed in bulk at `Drop`.
struct Chunk {
    base: *mut u8,
    layout: Layout,
}

struct ArenaState {
    chunks: Vec<Chunk>,
    /// Base pointer of the chunk currently being bumped (null before the first).
    cur: *mut u8,
    /// Capacity in bytes of the current chunk.
    cur_cap: usize,
    /// Bump offset within the current chunk.
    off: usize,
    /// Total bytes consumed across all chunks (drives the cap fallback).
    total: usize,
    /// User pointer of the most recent arena allocation, for in-place realloc.
    last_user: *mut u8,
    /// Bump offset immediately after `last_user`'s user data.
    last_end: usize,
}

impl ArenaState {
    /// Reserve `bytes` in a fresh chunk sized to at least `bytes`, aligned to
    /// `align`, and make it current. Returns false only on allocation failure.
    fn grow(&mut self, bytes: usize, align: usize) -> bool {
        let cap = bytes.max(CHUNK_SIZE);
        let layout = match Layout::from_size_align(cap, align.max(CHUNK_ALIGN)) {
            Ok(layout) => layout,
            Err(_) => return false,
        };
        // SAFETY: layout has non-zero size (cap >= CHUNK_SIZE).
        let base = unsafe { std::alloc::alloc(layout) };
        if base.is_null() {
            return false;
        }
        #[cfg(test)]
        CHUNK_ALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.total += layout.size();
        self.chunks.push(Chunk { base, layout });
        self.cur = base;
        self.cur_cap = layout.size();
        self.off = 0;
        true
    }
}

/// A request-scoped bump arena. Shared via `Arc`; released in bulk when the last
/// reference drops.
pub struct RequestArena {
    state: Mutex<ArenaState>,
}

// SAFETY: all interior mutation goes through the `Mutex`, and the raw chunk
// pointers are owned solely by this arena (allocated in `grow`, freed once in
// `Drop`). No chunk pointer escapes except as user pointers into arena memory,
// whose lifetime is bounded by the arena's own.
unsafe impl Send for RequestArena {}
unsafe impl Sync for RequestArena {}

impl RequestArena {
    // Minted per request by the lifecycle wiring (A2, router.rs); used by tests here.
    #[allow(dead_code)]
    pub fn new() -> Arc<Self> {
        Arc::new(RequestArena {
            state: Mutex::new(ArenaState {
                chunks: Vec::new(),
                cur: core::ptr::null_mut(),
                cur_cap: 0,
                off: 0,
                total: 0,
                last_user: core::ptr::null_mut(),
                last_end: 0,
            }),
        })
    }

    /// Bump-allocate `length` bytes at `alignment`, writing a tagged header.
    /// Returns `None` when the per-request cap is reached (caller falls through
    /// to the global allocator with a plain header).
    fn try_alloc(&self, length: usize, alignment: usize) -> Option<*mut c_void> {
        let hs = header_size(alignment);
        let total = length.checked_add(hs)?;
        let mut s = self.state.lock().expect("arena mutex poisoned");
        if s.total >= ARENA_CAP {
            return None;
        }
        // Try the current chunk; grow if the aligned user block doesn't fit.
        let user_ptr = self.carve(&mut s, total, hs, alignment)?;
        // SAFETY: `user_ptr` points `hs` bytes into freshly reserved chunk
        // space; the header word sits at `user_ptr - WORD`, inside the reservation.
        unsafe { write_size_word(user_ptr, total | TAG_ARENA) };
        Some(user_ptr as *mut c_void)
    }

    /// Reserve space for a `total`-byte allocation (header + `length`) whose user
    /// pointer is `alignment`-aligned, updating bump bookkeeping. `hs` is the
    /// header size. Returns the user pointer.
    fn carve(
        &self,
        s: &mut ArenaState,
        total: usize,
        hs: usize,
        alignment: usize,
    ) -> Option<*mut u8> {
        let length = total - hs;
        for attempt in 0..2 {
            if !s.cur.is_null() {
                let chunk_addr = s.cur as usize;
                // User pointer: aligned, with `hs` header bytes before it.
                let user_addr = align_up(chunk_addr + s.off + hs, alignment);
                let end = (user_addr - chunk_addr).checked_add(length)?;
                if end <= s.cur_cap {
                    s.off = end;
                    let user_ptr = user_addr as *mut u8;
                    s.last_user = user_ptr;
                    s.last_end = end;
                    return Some(user_ptr);
                }
            }
            if attempt == 0 {
                // Need a chunk big enough for header + alignment slack + data.
                let needed = total.checked_add(alignment)?;
                if !s.grow(needed, alignment) {
                    return None;
                }
            }
        }
        None
    }

    /// In-place grow/shrink when `ptr` is the arena's most recent allocation and
    /// the current chunk has room. Returns the same pointer on success.
    fn try_realloc_inplace(
        &self,
        ptr: *mut u8,
        new_length: usize,
        alignment: usize,
    ) -> Option<*mut c_void> {
        let hs = header_size(alignment);
        let total = new_length.checked_add(hs)?;
        let mut s = self.state.lock().expect("arena mutex poisoned");
        if ptr != s.last_user || s.cur.is_null() {
            return None;
        }
        let chunk_addr = s.cur as usize;
        let user_addr = ptr as usize;
        let new_end = (user_addr - chunk_addr).checked_add(new_length)?;
        if new_end > s.cur_cap {
            return None;
        }
        let old_end = s.last_end;
        s.off = new_end;
        s.last_end = new_end;
        if new_end > old_end {
            s.total += new_end - old_end;
        }
        // SAFETY: `ptr`'s header word is within the arena reservation.
        unsafe { write_size_word(ptr, total | TAG_ARENA) };
        Some(ptr as *mut c_void)
    }
}

impl Drop for RequestArena {
    fn drop(&mut self) {
        let s = self.state.get_mut().expect("arena mutex poisoned");
        for chunk in s.chunks.drain(..) {
            // SAFETY: `base`/`layout` are exactly what `grow` allocated, and no
            // user pointer into this chunk outlives the arena (the last `Arc`
            // drop is what runs this).
            unsafe { std::alloc::dealloc(chunk.base, chunk.layout) };
            #[cfg(test)]
            CHUNK_FREES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local current arena.
// ---------------------------------------------------------------------------

thread_local! {
    static CURRENT: RefCell<Option<Arc<RequestArena>>> = const { RefCell::new(None) };
}

/// Install `arena` as the current thread's arena, dropping any previously
/// installed one. Install is an unconditional overwrite (never a save/restore
/// stack): arenas do not nest, and a predecessor whose uninstall was skipped by
/// a panic unwinding across Roc frames becomes a bounded, self-healing leak
/// here rather than a dangling pointer.
// Called by the request lifecycle wiring (A2, router.rs); used by tests here.
#[allow(dead_code)]
pub fn install(arena: Arc<RequestArena>) {
    CURRENT.with(|slot| {
        *slot.borrow_mut() = Some(arena);
    });
}

/// Clear the current thread's arena, dropping the reference.
// Called by the request lifecycle wiring (A2, router.rs); used by tests here.
#[allow(dead_code)]
pub fn uninstall() {
    CURRENT.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

/// Run `f` with the current thread's arena, if any. The borrow is held for the
/// duration of `f`; `f` must not re-enter arena installation.
#[inline]
fn with_current<R>(f: impl FnOnce(Option<&RequestArena>) -> R) -> R {
    CURRENT.with(|slot| f(slot.borrow().as_deref()))
}

// ---------------------------------------------------------------------------
// The exported allocator entry points.
// ---------------------------------------------------------------------------

/// Arena-aware `roc_alloc`. Bumps from the current arena when one is installed
/// and under budget; otherwise the global allocator, unchanged.
#[inline]
pub fn roc_alloc(length: usize, alignment: usize) -> *mut c_void {
    if let Some(ptr) = with_current(|arena| arena.and_then(|a| a.try_alloc(length, alignment))) {
        return ptr;
    }
    abi::DefaultAllocators::roc_alloc(core::ptr::null_mut(), length, alignment)
}

/// Arena-aware `roc_dealloc`. Arena pointers are a no-op (bulk-freed at arena
/// drop); everything else goes to the global allocator.
///
/// # Safety
/// `ptr` must have been produced by `roc_alloc` with the same `alignment`.
#[inline]
pub unsafe fn roc_dealloc(ptr: *mut c_void, alignment: usize) {
    let word = unsafe { read_size_word(ptr as *const u8) };
    if word & TAG_ARENA != 0 {
        #[cfg(debug_assertions)]
        {
            assert!(
                word & TAG_FREED == 0,
                "double roc_dealloc of arena pointer {ptr:?}",
            );
            unsafe { write_size_word(ptr as *mut u8, word | TAG_FREED) };
        }
        return;
    }
    abi::DefaultAllocators::roc_dealloc(core::ptr::null_mut(), ptr, alignment);
}

/// Arena-aware `roc_realloc`.
///
/// # Safety
/// `ptr` must have been produced by `roc_alloc` with the same `alignment`.
#[inline]
pub unsafe fn roc_realloc(
    ptr: *mut c_void,
    new_length: usize,
    alignment: usize,
) -> *mut c_void {
    let word = unsafe { read_size_word(ptr as *const u8) };
    if word & TAG_ARENA == 0 {
        // Global-owned: hand straight to the global reallocator.
        return abi::DefaultAllocators::roc_realloc(
            core::ptr::null_mut(),
            ptr,
            new_length,
            alignment,
        );
    }

    let hs = header_size(alignment);
    let old_total = word & SIZE_MASK;
    let old_length = old_total - hs;
    let copy_len = old_length.min(new_length);

    // Fast path: extend the last allocation in place.
    if let Some(p) =
        with_current(|arena| arena.and_then(|a| a.try_realloc_inplace(ptr as *mut u8, new_length, alignment)))
    {
        return p;
    }

    // Fresh arena block + copy when an arena is installed and under budget.
    if let Some(dst) =
        with_current(|arena| arena.and_then(|a| a.try_alloc(new_length, alignment)))
    {
        // SAFETY: both regions are valid for `copy_len` bytes; distinct blocks.
        unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, dst as *mut u8, copy_len) };
        return dst;
    }

    // Cap reached, or reallocating an arena pointer from a thread with no arena
    // installed (defensive — Roc values are only grown on their request thread):
    // move to a fresh global block with a plain header. The old arena block is
    // abandoned and reclaimed at arena drop.
    let dst =
        abi::DefaultAllocators::roc_alloc(core::ptr::null_mut(), new_length, alignment) as *mut u8;
    // SAFETY: `dst` is a fresh `new_length` allocation; `ptr` holds `old_length`
    // readable bytes; the blocks are distinct.
    unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, dst, copy_len) };
    dst as *mut c_void
}

// ---------------------------------------------------------------------------
// `RocHost` fn-pointer table entries, so the host's own Roc-value helpers
// (built through `roc_host()`) route through the arena too when one is
// installed. Same behavior as the exported symbols; different ABI shape.
// ---------------------------------------------------------------------------

pub extern "C" fn host_alloc(
    _host: *mut abi::RocHost,
    length: usize,
    alignment: usize,
) -> *mut c_void {
    roc_alloc(length, alignment)
}

pub extern "C" fn host_dealloc(_host: *mut abi::RocHost, ptr: *mut c_void, alignment: usize) {
    // SAFETY: forwarded from a host helper that allocated `ptr` via `host_alloc`.
    unsafe { roc_dealloc(ptr, alignment) };
}

pub extern "C" fn host_realloc(
    _host: *mut abi::RocHost,
    ptr: *mut c_void,
    new_length: usize,
    alignment: usize,
) -> *mut c_void {
    // SAFETY: forwarded from a host helper that allocated `ptr` via `host_alloc`.
    unsafe { roc_realloc(ptr, new_length, alignment) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    const BIG_TEXT: &[u8] = b"a string long enough to defeat the small-string optimization";

    /// Serializes the arena tests so the process-wide chunk counters yield
    /// attributable deltas. Poison-tolerant: a `#[should_panic]`-style test that
    /// trips the debug guard must not wedge the rest of the suite.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tagged(ptr: *const c_void) -> bool {
        (unsafe { read_size_word(ptr as *const u8) } & TAG_ARENA) != 0
    }

    #[test]
    fn alloc_is_arena_tagged_dealloc_is_noop_drop_frees() {
        let _g = serial();
        let a0 = CHUNK_ALLOCS.load(Ordering::Relaxed);
        let f0 = CHUNK_FREES.load(Ordering::Relaxed);
        {
            let arena = RequestArena::new();
            install(arena.clone());
            let p = roc_alloc(100, 8) as *mut u8;
            assert!(tagged(p as *const c_void), "arena alloc must carry the arena tag");
            for i in 0..100u8 {
                unsafe { *p.add(i as usize) = i };
            }
            // dealloc is a no-op: the bytes stay live until the arena drops.
            unsafe { roc_dealloc(p as *mut c_void, 8) };
            for i in 0..100u8 {
                assert_eq!(unsafe { *p.add(i as usize) }, i, "arena bytes freed too early");
            }
            uninstall();
        }
        let allocs = CHUNK_ALLOCS.load(Ordering::Relaxed) - a0;
        let frees = CHUNK_FREES.load(Ordering::Relaxed) - f0;
        assert!(allocs >= 1, "expected at least one chunk");
        assert_eq!(allocs, frees, "arena leaked chunks on drop");
    }

    #[test]
    fn align16_realloc_grow_in_place_preserves_contents() {
        let _g = serial();
        let arena = RequestArena::new();
        install(arena.clone());
        let p = roc_alloc(16, 16) as *mut u8;
        assert_eq!(p as usize % 16, 0, "16-aligned request must return 16-aligned ptr");
        for i in 0..16u8 {
            unsafe { *p.add(i as usize) = i + 1 };
        }
        // p is the last allocation, so this grows in place (same pointer).
        let p2 = unsafe { roc_realloc(p as *mut c_void, 64, 16) } as *mut u8;
        assert_eq!(p2 as usize % 16, 0);
        assert!(tagged(p2 as *const c_void));
        for i in 0..16u8 {
            assert_eq!(unsafe { *p2.add(i as usize) }, i + 1, "grow lost the original bytes");
        }
        uninstall();
    }

    #[test]
    fn realloc_when_not_last_copies_into_fresh_block() {
        let _g = serial();
        let arena = RequestArena::new();
        install(arena.clone());
        let p = roc_alloc(16, 16) as *mut u8;
        for i in 0..16u8 {
            unsafe { *p.add(i as usize) = i + 100 };
        }
        // A second alloc means p is no longer the bump tail; realloc must copy.
        let _q = roc_alloc(16, 16);
        let p2 = unsafe { roc_realloc(p as *mut c_void, 48, 16) } as *mut u8;
        assert_ne!(p2, p, "non-tail realloc should move to a fresh block");
        assert!(tagged(p2 as *const c_void));
        for i in 0..16u8 {
            assert_eq!(unsafe { *p2.add(i as usize) }, i + 100, "copy lost the original bytes");
        }
        uninstall();
    }

    #[test]
    fn realloc_of_arena_ptr_without_arena_falls_to_global_untagged() {
        let _g = serial();
        let arena = RequestArena::new();
        install(arena.clone());
        let p = roc_alloc(24, 16) as *mut u8;
        for i in 0..24u8 {
            unsafe { *p.add(i as usize) = i + 7 };
        }
        // Drop the TLS arena: now a realloc of the (still-tagged) pointer has no
        // arena to bump from and must move to a plain global block.
        uninstall();
        let p2 = unsafe { roc_realloc(p as *mut c_void, 80, 16) } as *mut u8;
        assert!(!tagged(p2 as *const c_void), "global-owned realloc must have a plain header");
        for i in 0..24u8 {
            assert_eq!(unsafe { *p2.add(i as usize) }, i + 7, "global copy lost bytes");
        }
        unsafe { roc_dealloc(p2 as *mut c_void, 16) }; // frees the global block
        // p's original arena block is reclaimed when `arena` drops here.
        drop(arena);
    }

    #[test]
    fn cap_boundary_allocation_is_untagged_global() {
        let _g = serial();
        let arena = RequestArena::new();
        install(arena.clone());
        assert!(tagged(roc_alloc(8, 8)), "pre-cap alloc is arena-owned");
        // Exhaust the per-request cap with chunk-sized allocations.
        let mut over = core::ptr::null_mut::<c_void>();
        for _ in 0..(ARENA_CAP / CHUNK_SIZE + 2) {
            over = roc_alloc(CHUNK_SIZE, 8);
        }
        assert!(!tagged(over), "past the cap, allocations fall through to the global heap");
        unsafe { roc_dealloc(over, 8) }; // global free of the post-cap block
        uninstall();
    }

    #[test]
    fn foreign_thread_dealloc_of_arena_ptr_is_a_noop() {
        let _g = serial();
        let arena = RequestArena::new();
        install(arena.clone());
        let p = roc_alloc(100, 8) as *mut u8;
        unsafe { *p = 42 };
        uninstall();
        // A thread with no arena installed must treat the tagged pointer as a
        // no-op, never handing it to the global allocator.
        let addr = p as usize;
        std::thread::spawn(move || unsafe { roc_dealloc(addr as *mut c_void, 8) })
            .join()
            .unwrap();
        assert_eq!(unsafe { *p }, 42, "foreign-thread dealloc must not disturb arena memory");
        drop(arena);
    }

    /// on_drop for an erased callable whose 24-byte capture is a single RocStr.
    extern "C" fn drop_captured_rocstr(capture: *mut u8, host: *mut abi::RocHost) {
        unsafe {
            let s = core::ptr::read(capture as *const abi::RocStr);
            s.decref(&*host);
        }
    }
    extern "C" fn noop_callable(_: *mut abi::RocHost, _: *mut u8, _: *const u8, _: *mut u8) {}

    #[test]
    fn erased_callable_teardown_from_no_tls_thread() {
        let _g = serial();
        let host = crate::roc_host(); // rerouted through the arena
        let arena = RequestArena::new();
        install(arena.clone());

        // Build a boxed closure capturing an arena-allocated (big) RocStr.
        let capture_size = core::mem::size_of::<abi::RocStr>();
        let callable = unsafe {
            abi::roc_erased_callable_allocate(
                host,
                noop_callable,
                Some(drop_captured_rocstr),
                capture_size,
            )
        };
        let s = abi::RocStr::from_slice(BIG_TEXT, host);
        assert!(!s.is_small_str(), "capture must be a heap (arena) string");
        unsafe {
            let cap = abi::roc_erased_callable_capture_ptr(callable);
            core::ptr::write(cap as *mut abi::RocStr, s);
        }
        uninstall();

        // Final decref on a thread with no arena: the on_drop decrefs the
        // captured arena string and frees the arena-backed box — both must be
        // no-ops that never touch the global allocator.
        let addr = callable as usize;
        std::thread::spawn(move || unsafe {
            abi::decref_erased_callable(addr as *mut u8, crate::roc_host());
        })
        .join()
        .unwrap();

        drop(arena); // reclaims the box and the string in one shot
    }

    #[test]
    fn install_overwrites_and_drops_the_stale_arena() {
        let _g = serial();
        let a1 = RequestArena::new();
        install(a1.clone());
        let _p = roc_alloc(64, 8); // force a chunk so a1 owns memory
        let weak = Arc::downgrade(&a1);
        drop(a1); // now only the TLS slot references a1

        // Install a fresh arena WITHOUT uninstalling first (simulates a panic
        // that skipped the previous request's uninstall). The overwrite must
        // drop the stale arena rather than leak or dangle it.
        let a2 = RequestArena::new();
        install(a2.clone());
        assert!(weak.upgrade().is_none(), "stale arena must be dropped on overwrite-install");
        uninstall();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn double_dealloc_of_arena_ptr_trips_the_debug_guard() {
        let _g = serial();
        let arena = RequestArena::new();
        install(arena.clone());
        let p = roc_alloc(32, 8);
        unsafe { roc_dealloc(p, 8) }; // first: marks the header freed
        let addr = p as usize;
        let second = std::panic::catch_unwind(|| unsafe { roc_dealloc(addr as *mut c_void, 8) });
        uninstall(); // always runs, keeping TLS clean for pooled test threads
        assert!(second.is_err(), "a second dealloc of an arena pointer must panic in debug");
        drop(arena);
    }
}
