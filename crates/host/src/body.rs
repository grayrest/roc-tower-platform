//! Unified response body type for the middleware stack.
//!
//! Every layer seam (compression, static files, Roc responses, error pages)
//! boxes into `AppBody`, and CORS requires `Default` for preflight
//! short-circuits, which the raw `BoxBody` combinator does not provide.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};

use crate::abi;
use crate::arena::RequestArena;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Aggregate arena bytes currently pinned by in-flight zero-copy response
/// bodies. Bounds how much memory slow readers can hold hostage: once the gauge
/// would exceed [`MAX_IN_FLIGHT_ARENA_BYTES`], new responses copy their body and
/// release the arena immediately instead of borrowing it.
static IN_FLIGHT_ARENA_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Cap on aggregate pinned arena memory before falling back to copies.
const MAX_IN_FLIGHT_ARENA_BYTES: usize = 128 * 1024 * 1024;

/// Don't pin a whole request arena to stream a body far smaller than it — past
/// this arena-reserved-to-body-length ratio, copy the body instead. (Both
/// bounds are deliberately generous defaults, revisited by the A4 bench.)
const MAX_ARENA_TO_BODY_RATIO: usize = 32;

/// Owns a response body's Roc list plus the arena backing its bytes, letting
/// hyper stream straight out of arena memory with no copy. `Bytes::from_owner`
/// keeps this alive until the body has been fully written.
pub struct RocBodyOwner {
    /// Declared before `arena`: `Drop::drop` decrefs the list (a no-op for
    /// arena bytes; the real free for a cap-fallback global body) while the
    /// arena is still mapped, and the arena reference releases only afterward.
    body: abi::RocListWith<u8, false>,
    /// Kept solely to hold the backing region mapped for the body's lifetime;
    /// never read directly.
    #[allow(dead_code)]
    arena: Arc<RequestArena>,
    /// Arena bytes this owner added to the in-flight gauge, subtracted on drop.
    held_bytes: usize,
}

// SAFETY: the body list is an owned Roc reference (atomic refcounts) into memory
// kept alive by `arena`; nothing aliases it mutably. The raw element pointer is
// only ever read as bytes.
unsafe impl Send for RocBodyOwner {}

impl AsRef<[u8]> for RocBodyOwner {
    fn as_ref(&self) -> &[u8] {
        self.body.as_slice()
    }
}

impl Drop for RocBodyOwner {
    fn drop(&mut self) {
        // SAFETY: this owner holds exactly one reference to `body`; the response
        // struct's copy was neutralized before its recursive decref (roc_call).
        unsafe { self.body.decref(crate::roc_host()) };
        IN_FLIGHT_ARENA_BYTES.fetch_sub(self.held_bytes, Ordering::Relaxed);
    }
}

/// A marshaled response body: either borrowed from arena-backed Roc memory
/// (zero-copy) or an owned copy (error-handler replies, or the slow-drain and
/// no-arena fallbacks).
pub enum ResponseBody {
    Roc(RocBodyOwner),
    Copied(Vec<u8>),
}

impl ResponseBody {
    /// Take ownership of a handler's response body list. Borrows it zero-copy
    /// from the arena when that is worthwhile and within the in-flight budget;
    /// otherwise copies the bytes and releases the list immediately.
    ///
    /// # Safety
    /// `body` must be a live, singly-owned Roc list whose bytes live in `arena`
    /// (or in a global cap-fallback block), and whose reference this call takes
    /// over (the caller must have neutralized any other copy of it).
    pub unsafe fn from_roc(body: abi::RocListWith<u8, false>, arena: Arc<RequestArena>) -> Self {
        let body_len = body.as_slice().len();
        let reserved = arena.reserved_bytes();
        let would_pin = IN_FLIGHT_ARENA_BYTES.load(Ordering::Relaxed) + reserved;
        let worth_borrowing = body_len != 0
            && reserved <= body_len.saturating_mul(MAX_ARENA_TO_BODY_RATIO)
            && would_pin <= MAX_IN_FLIGHT_ARENA_BYTES;

        if !worth_borrowing {
            let copy = body.as_slice().to_vec();
            // SAFETY: caller transferred the sole reference to `body`.
            unsafe { body.decref(crate::roc_host()) };
            return ResponseBody::Copied(copy);
        }

        IN_FLIGHT_ARENA_BYTES.fetch_add(reserved, Ordering::Relaxed);
        ResponseBody::Roc(RocBodyOwner {
            body,
            arena,
            held_bytes: reserved,
        })
    }

    pub fn into_bytes(self) -> Bytes {
        match self {
            ResponseBody::Roc(owner) => Bytes::from_owner(owner),
            ResponseBody::Copied(bytes) => Bytes::from(bytes),
        }
    }
}

impl From<Vec<u8>> for ResponseBody {
    fn from(bytes: Vec<u8>) -> Self {
        ResponseBody::Copied(bytes)
    }
}

pub struct AppBody(UnsyncBoxBody<Bytes, BoxError>);

impl AppBody {
    pub fn new<B>(body: B) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        Self(body.map_err(Into::into).boxed_unsync())
    }

    pub fn empty() -> Self {
        Self::new(Empty::new())
    }

    pub fn full(bytes: impl Into<Bytes>) -> Self {
        Self::new(Full::new(bytes.into()))
    }
}

impl Default for AppBody {
    fn default() -> Self {
        Self::empty()
    }
}

impl Body for AppBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.0).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.0.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::test_serial;
    use crate::roc_host;

    /// Build a Roc `List(U8)` of `len` bytes (0,1,2,… mod 256) through the
    /// (arena-routed) host allocator, returning the list and the expected bytes.
    fn arena_body(len: usize) -> (abi::RocListWith<u8, false>, Vec<u8>) {
        let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let list = unsafe { abi::RocListWith::<u8, false>::from_slice(&data, roc_host()) };
        (list, data)
    }

    #[test]
    fn large_arena_body_is_borrowed_zero_copy_and_released_on_drop() {
        let _g = test_serial();
        assert_eq!(IN_FLIGHT_ARENA_BYTES.load(Ordering::Relaxed), 0);
        let arena = RequestArena::new();
        crate::arena::install(arena.clone());
        // A body large relative to the arena's single chunk stays zero-copy.
        let (list, data) = arena_body(32 * 1024);
        let body = unsafe { ResponseBody::from_roc(list, arena.clone()) };
        assert!(matches!(body, ResponseBody::Roc(_)), "large arena body should borrow");
        assert!(IN_FLIGHT_ARENA_BYTES.load(Ordering::Relaxed) > 0, "gauge should reflect the pin");
        let bytes = body.into_bytes();
        assert_eq!(bytes.as_ref(), data.as_slice(), "borrowed body must be byte-identical");
        drop(bytes); // releases the owner -> decref + gauge subtract
        assert_eq!(IN_FLIGHT_ARENA_BYTES.load(Ordering::Relaxed), 0, "gauge leaked");
        crate::arena::uninstall();
    }

    #[test]
    fn tiny_body_is_copied_and_does_not_pin_the_arena() {
        let _g = test_serial();
        let arena = RequestArena::new();
        crate::arena::install(arena.clone());
        let (list, data) = arena_body(16); // tiny vs a 64 KiB chunk -> copy
        let body = unsafe { ResponseBody::from_roc(list, arena.clone()) };
        assert!(matches!(body, ResponseBody::Copied(_)), "tiny body should copy");
        assert_eq!(IN_FLIGHT_ARENA_BYTES.load(Ordering::Relaxed), 0, "copy must not pin the arena");
        assert_eq!(body.into_bytes().as_ref(), data.as_slice());
        crate::arena::uninstall();
    }

    #[test]
    fn empty_body_copies() {
        let _g = test_serial();
        let arena = RequestArena::new();
        crate::arena::install(arena.clone());
        let (list, _) = arena_body(0);
        let body = unsafe { ResponseBody::from_roc(list, arena.clone()) };
        assert!(matches!(body, ResponseBody::Copied(_)));
        assert!(body.into_bytes().is_empty());
        crate::arena::uninstall();
    }

    /// The cap-fallback double-free canary: once the arena is over budget, the
    /// response body lands in a *global* (untagged) block. The handler path
    /// neutralizes the response's own copy of the list before its recursive
    /// decref, so the body owner is the sole owner and frees the global block
    /// exactly once. Simulate both the neutralized-field decref and the owner
    /// drop; a missing neutralization would double-free the global block and
    /// abort the test process.
    #[test]
    fn cap_fallback_global_body_is_freed_exactly_once() {
        let _g = test_serial();
        let arena = RequestArena::new();
        crate::arena::install(arena.clone());

        // Exhaust the per-request cap (4 MiB in 64 KiB chunks) so the next body
        // allocation falls through to the global allocator with a plain header.
        for _ in 0..80 {
            crate::arena::roc_alloc(64 * 1024, 8);
        }
        // Large enough (relative to the ~5 MiB of exhausted arena) to stay on the
        // zero-copy borrow path even though its bytes are a global block — the
        // exact case where a stray response.decref would double-free.
        let (list, data) = arena_body(256 * 1024);

        // Handler path: take the list by Copy, neutralize the response's field.
        let body_list = list;
        let neutralized = abi::RocListWith::<u8, false>::empty();

        let body = unsafe { ResponseBody::from_roc(body_list, arena.clone()) };
        assert!(matches!(body, ResponseBody::Roc(_)), "large global body should still borrow");
        // response.decref would touch only the neutralized (empty) field: no-op.
        unsafe { neutralized.decref(roc_host()) };

        let bytes = body.into_bytes();
        assert_eq!(bytes.as_ref(), data.as_slice());
        drop(bytes); // frees the global block once; no abort => no double free
        crate::arena::uninstall();
    }

    /// Whichever of {ctx guard, response body owner} drops last is the one that
    /// frees the arena region. Both orderings must free it exactly once with no
    /// chunk left behind.
    fn arena_freed_after_last_holder(reaper_first: bool) {
        use crate::arena::{CHUNK_ALLOCS, CHUNK_FREES, CHUNK_RECYCLES, CHUNK_REUSES};
        let _g = test_serial();
        let a0 = CHUNK_ALLOCS.load(Ordering::Relaxed);
        let f0 = CHUNK_FREES.load(Ordering::Relaxed);
        let u0 = CHUNK_REUSES.load(Ordering::Relaxed);
        let c0 = CHUNK_RECYCLES.load(Ordering::Relaxed);

        let arena = RequestArena::new();
        crate::arena::install(arena.clone());
        let (list, _) = arena_body(32 * 1024);
        let body = unsafe { ResponseBody::from_roc(list, arena.clone()) };
        assert!(matches!(body, ResponseBody::Roc(_)), "test needs the borrow path");
        let ctx = crate::effects::mint_ctx(arena.clone(), bytes::Bytes::new(), Vec::new());
        crate::arena::uninstall();

        // Drop the dispatch-local reference; now only the ctx guard and the body
        // owner keep the region alive.
        let weak = Arc::downgrade(&arena);
        drop(arena);

        // Drop whichever holder goes first; the region must survive until the
        // second (last) holder drops, in either order.
        if reaper_first {
            drop(ctx);
            assert!(weak.upgrade().is_some(), "region freed before the last holder dropped");
            drop(body);
        } else {
            drop(body);
            assert!(weak.upgrade().is_some(), "region freed before the last holder dropped");
            drop(ctx);
        }
        assert!(weak.upgrade().is_none(), "region not freed after the last holder dropped");

        // A chunk that came in (allocated OR reused from the warm pool) must go
        // out (freed OR recycled to the pool) — no leak, with pooling on.
        let allocs = CHUNK_ALLOCS.load(Ordering::Relaxed) - a0;
        let frees = CHUNK_FREES.load(Ordering::Relaxed) - f0;
        let reuses = CHUNK_REUSES.load(Ordering::Relaxed) - u0;
        let recycles = CHUNK_RECYCLES.load(Ordering::Relaxed) - c0;
        assert!(allocs + reuses >= 1);
        assert_eq!(allocs + reuses, frees + recycles, "arena leaked chunks");
    }

    #[test]
    fn arena_freed_when_ctx_guard_drops_before_body_owner() {
        arena_freed_after_last_holder(true);
    }

    #[test]
    fn arena_freed_when_body_owner_drops_before_ctx_guard() {
        arena_freed_after_last_holder(false);
    }
}
