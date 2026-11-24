//! Shared JSON-extraction helpers for Microsoft Graph object normalization.

use engine_core::{ids::IdError, time::UtcDateTime};
use serde_json::Value;

use crate::error::GraphError;

/// A required string field, or a protocol error naming it.
pub(crate) fn req_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, GraphError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| GraphError::protocol(format!("missing string field {key:?}")))
}

/// An optional string field (absent for JSON `null` or a missing key).
pub(crate) fn opt_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A boolean field, defaulting to `false` when absent or non-boolean.
pub(crate) fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Parses an ISO-8601 instant field (Graph `…Z`), or `None` for absent/null.
pub(crate) fn datetime(value: &Value, key: &str) -> Result<Option<UtcDateTime>, GraphError> {
    match value.get(key).and_then(Value::as_str) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<UtcDateTime>()
            .map(Some)
            .map_err(|e| GraphError::protocol(format!("bad {key} datetime {raw:?}: {e}"))),
    }
}

/// Wraps an id-construction result, naming the field on failure.
pub(crate) fn wrap_id<T>(result: Result<T, IdError>, what: &str) -> Result<T, GraphError> {
    result.map_err(|e| GraphError::protocol(format!("bad {what}: {e}")))
}

/// The SHA-256 of `text`, in lowercase hex: the version of a setting Graph keeps no ETag on.
pub(crate) fn digest_hex(text: &str) -> String {
    use core::fmt::Write as _;

    use sha2::Digest as _;

    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
