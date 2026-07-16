//! Conversion between host HTTP types and Roc records, plus invocation of
//! boxed Roc handlers through the erased-callable ABI.

use crate::abi;
use crate::roc_host;
use crate::types::{RocKeyValue, RocRequest, RocResponse};

/// A boxed Roc handler function. The route list returned by `roc_app` is
/// intentionally never released, so the callable lives for the whole process.
#[derive(Clone, Copy)]
pub struct RocHandler(pub(crate) abi::RocErasedCallable);

unsafe impl Send for RocHandler {}
unsafe impl Sync for RocHandler {}

/// The app's Model, produced once by `roc_init`. Held for the process
/// lifetime; each handler call transfers one retained reference in.
#[derive(Clone, Copy)]
pub struct RocModel(pub(crate) abi::RocBox);

unsafe impl Send for RocModel {}
unsafe impl Sync for RocModel {}

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
    abi::RocStr::from_slice(text.as_bytes(), roc_host())
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
    let host = roc_host();
    unsafe {
        let list = abi::RocList::<RocKeyValue>::allocate(pairs.len(), host);
        for (index, (name, value)) in pairs.iter().enumerate() {
            list.elements.add(index).write(RocKeyValue {
                name: roc_str(name),
                value: roc_str(value),
            });
        }
        list
    }
}

/// Call `roc_init` once at startup and unwrap the app's Model.
pub fn init_model() -> Result<RocModel, String> {
    let result = unsafe { abi::roc_init() };
    match result.tag {
        abi::InitForHostResultTag::Ok => Ok(RocModel(result.payload_ok())),
        abi::InitForHostResultTag::Err => {
            let message = result.payload_err();
            let text = String::from_utf8_lossy(message.as_slice()).into_owned();
            unsafe { message.decref(roc_host()) };
            Err(text)
        }
    }
}

/// Invoke a boxed Roc handler.
///
/// Ownership: the args buffer (request record carrying one retained Model
/// reference) is consumed by the callee; the returned `RocResponse` is owned
/// by us and released after copying out.
///
/// `ctx` is the per-request coeffect-memo registry id minted by the router.
/// The router owns the teardown guard — it must NOT live in this frame: a
/// panic that unwinds out of a hosted fn crosses the Roc frames without
/// running the Rust cleanups below them (observed on macOS aarch64), so a
/// guard here would leak on panic. On the async side of `spawn_blocking` the
/// `JoinError` path is ordinary Rust control flow and the guard is reliable.
pub fn call_handler(
    handler: RocHandler,
    model: RocModel,
    parts: RequestParts,
    ctx: u64,
) -> (ResponseParts, crate::effect::DeferredEffects) {
    #[repr(C)]
    struct Args {
        arg0: RocRequest,
    }

    let host = roc_host();
    unsafe { abi::incref_box(model.0, 1) };
    let args = Args {
        arg0: RocRequest {
            ctx,
            headers: roc_kv_list(&parts.headers),
            method: roc_str(&parts.method),
            model: model.0,
            params: roc_kv_list(&parts.params),
            path: roc_str(&parts.path),
            query: roc_str(&parts.query),
        },
    };

    let mut ret = core::mem::MaybeUninit::<RocResponse>::uninit();
    unsafe {
        let payload = abi::roc_erased_callable_payload_ptr(handler.0);
        let capture = abi::roc_erased_callable_capture_ptr(handler.0);
        ((*payload).callable_fn_ptr)(
            host as *const abi::RocHost as *mut abi::RocHost,
            ret.as_mut_ptr() as *mut u8,
            &args as *const Args as *const u8,
            capture,
        );
        let mut response = ret.assume_init();

        // Hand the body's Roc list to a `ResponseBody` owner so hyper can stream
        // straight out of arena memory. Take the list out and neutralize the
        // response's copy FIRST: `response.decref` below recursively decrefs the
        // body field, and `RocListWith` is `Copy`, so without this an arena body
        // would be a harmless double no-op but a >4 MiB cap-fallback (global)
        // body would be genuinely freed out from under the owner.
        let body_list = response.body;
        response.body = abi::RocListWith::empty();
        let body = match crate::arena::current_arena() {
            // SAFETY: `body_list` is the handler's sole live reference to the
            // response body (just neutralized in `response`), and it was built
            // this request, so its bytes live in `arena` (or a global fallback).
            Some(arena) => crate::body::ResponseBody::from_roc(body_list, arena),
            None => {
                let copy = body_list.as_slice().to_vec();
                body_list.decref(host);
                crate::body::ResponseBody::Copied(copy)
            }
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
        let (out, deferred) = crate::effect::run_effects(&response, handler_response);

        // One recursive decref frees the whole response, including each effect's
        // tag payload and boxed err_handler (erased callable). The deferred
        // `Opt`s hold owned copies, so they survive this decref.
        response.decref(host);
        (out, deferred)
    }
}
