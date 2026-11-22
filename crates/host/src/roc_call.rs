//! Conversion between host HTTP types and Roc records, plus invocation of
//! the app's boxed Roc handlers.

use crate::abi;
use crate::types::{RocHandler, RocKeyValue, RocRequest};

/// The routing-pure request data handed to Roc. The BODY and the Cookie
/// header are deliberately absent: they live in the ctx registry and are
/// extracted via `Host.request_body!`/`Host.cookie!`, which is what lets the
/// response cache know which routes vary on them (D13/D14).
pub struct RequestParts {
    pub method: String,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub query: String,
    pub headers: Vec<(String, String)>,
}

pub struct ResponseParts {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: crate::body::ResponseBody,
}

pub(crate) fn roc_str(text: &str) -> abi::RocStr {
    abi::RocStr::from_str(text)
}

/// Copy a Roc `List({ name : Str, value : Str })` into owned host pairs.
pub(crate) fn copy_headers(headers: &[RocKeyValue]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|kv| {
            (
                String::from_utf8_lossy(kv.name.as_slice()).into_owned(),
                String::from_utf8_lossy(kv.value.as_slice()).into_owned(),
            )
        })
        .collect()
}

/// Build a Roc `List({ name : Str, value : Str })` from host pairs.
pub(crate) fn roc_kv_list(pairs: &[(String, String)]) -> abi::RocList<RocKeyValue> {
    pairs
        .iter()
        .map(|(name, value)| RocKeyValue { name: roc_str(name), value: roc_str(value) })
        .collect()
}

/// Call `roc_init` once at startup and unwrap the app's Model. Only Roc can
/// release the model, so it is held for the process lifetime and each handler
/// call hands Roc one more reference.
pub fn init_model() -> Result<abi::RocOpaqueBox, String> {
    match abi::roc_init().into_view() {
        abi::InitForHostResultOwnedView::Ok(model) => Ok(model),
        abi::InitForHostResultOwnedView::Err(message) => Err(message.as_str().to_owned()),
    }
}

/// Invoke a boxed Roc handler.
///
/// Ownership: `model` is one retained Model reference; it moves into the call
/// with the request record that carries it; the returned response is ours, and its fields are released
/// as they drop once copied out.
///
/// `ctx` is the per-request coeffect-memo registry id minted by the router.
/// The router owns the teardown guard — it must NOT live in this frame: a
/// panic that unwinds out of a hosted fn crosses the Roc frames without
/// running the Rust cleanups below them (observed on macOS aarch64), so a
/// guard here would leak on panic. On the async side of `spawn_blocking` the
/// `JoinError` path is ordinary Rust control flow and the guard is reliable.
pub fn call_handler(
    handler: &RocHandler,
    model: abi::RocOpaqueBox,
    parts: RequestParts,
    ctx: u64,
) -> (ResponseParts, crate::effect::DeferredEffects) {
    let request = RocRequest {
        ctx,
        headers: roc_kv_list(&parts.headers),
        method: roc_str(&parts.method),
        model,
        params: roc_kv_list(&parts.params),
        path: roc_str(&parts.path),
        query: roc_str(&parts.query),
    };
    let response = handler.call(request);

    // Hand the body's Roc list to a `ResponseBody` owner so hyper can stream
    // straight out of arena memory.
    let body = match crate::arena::current_arena() {
        // The list was built this request, so its bytes live in `arena` (or a
        // global fallback past the arena's cap).
        Some(arena) => crate::body::ResponseBody::from_roc(response.body, arena),
        None => crate::body::ResponseBody::Copied(response.body.as_slice().to_vec()),
    };
    let handler_response = ResponseParts {
        status: response.status,
        headers: copy_headers(response.headers.as_slice()),
        body,
    };

    // Host executes the critical effects the handler returned (Arch B),
    // inside this spawn_blocking thread (S2): DB `Must` writes run first in
    // one `BEGIN IMMEDIATE` transaction, external `Must` writes after
    // commit. A failing `Must` replaces the response with its boxed
    // err_handler's reply. The fire-and-forget `Opt` effects come back as
    // `DeferredEffects` for the router to run post-flush (B3). An empty
    // effects list yields empty deferred effects, so the escape-hatch
    // (`effects: []`) path behaves exactly as before.
    //
    // The effects list, with each effect's payload and boxed err_handler, is
    // released when it drops at the end of this call; the deferred `Opt`s
    // hold owned copies.
    crate::effect::run_effects(&response.effects, handler_response)
}
