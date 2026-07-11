//! Conversion between host HTTP types and Roc records, plus invocation of
//! boxed Roc handlers through the erased-callable ABI.

use bytes::Bytes;

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

pub struct RequestParts {
    pub method: String,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

pub struct ResponseParts {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub(crate) fn roc_str(text: &str) -> abi::RocStr {
    abi::RocStr::from_slice(text.as_bytes(), roc_host())
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
pub fn call_handler(handler: RocHandler, model: RocModel, parts: RequestParts) -> ResponseParts {
    #[repr(C)]
    struct Args {
        arg0: RocRequest,
    }

    let host = roc_host();
    unsafe { abi::incref_box(model.0, 1) };
    let args = Args {
        arg0: RocRequest {
            body: unsafe { abi::RocListWith::<u8, false>::from_slice(&parts.body, host) },
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
        let response = ret.assume_init();
        let out = ResponseParts {
            status: response.status,
            headers: response
                .headers
                .as_slice()
                .iter()
                .map(|kv| {
                    (
                        String::from_utf8_lossy(kv.name.as_slice()).into_owned(),
                        String::from_utf8_lossy(kv.value.as_slice()).into_owned(),
                    )
                })
                .collect(),
            body: response.body.as_slice().to_vec(),
        };
        response.decref(host);
        out
    }
}
