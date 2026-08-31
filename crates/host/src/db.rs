//! Engine-neutral DB param and error types shared by the rusqlite (default) and
//! turso (`--features turso`) backends.
//!
//! Roc hands the host `SqlValue` params and expects a classified error back.
//! Decoding those into engine-neutral types here means the effect layer and the
//! cursor FFI glue never mention a specific engine — only the backend that binds
//! params and runs statements does.

use crate::abi;

/// A bound query parameter, decoded once from the Roc `Host.SqlValue` and
/// converted to the active engine's value type at bind time.
#[derive(Clone, Debug)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// Decode a Roc `Host.SqlValue` into the neutral param type.
pub fn param_from_host(value: &abi::HostSqlValue) -> SqlValue {
    match value.tag {
        abi::HostSqlValueTag::Null => SqlValue::Null,
        // SAFETY for every `_unchecked` below: the match arm has already
        // established the tag. These borrow rather than take: the payload
        // stays owned by `value`, and each arm copies out into owned Rust.
        abi::HostSqlValueTag::Integer => {
            SqlValue::Int(unsafe { *value.borrow_payload_integer_unchecked() })
        }
        abi::HostSqlValueTag::Real => {
            SqlValue::Real(unsafe { *value.borrow_payload_real_unchecked() })
        }
        abi::HostSqlValueTag::Text => SqlValue::Text(
            String::from_utf8_lossy(unsafe { value.borrow_payload_text_unchecked() }.as_slice())
                .into_owned(),
        ),
        abi::HostSqlValueTag::Blob => SqlValue::Blob(
            unsafe { value.borrow_payload_blob_unchecked() }.as_slice().to_vec(),
        ),
    }
}

/// Engine-neutral DB error classification returned by a backend's write path;
/// the effect layer maps it onto the public `Effect.Err`.
#[derive(Debug)]
pub enum DbError {
    UniqueViolation(String),
    ForeignKeyViolation(String),
    Busy,
    Other(String),
}

/// One write in a `Must` transaction: SQL plus its ordered bound params.
pub type Write = (String, Vec<SqlValue>);
