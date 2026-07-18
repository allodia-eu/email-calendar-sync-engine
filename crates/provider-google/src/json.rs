//! Shared JSON-extraction helpers for Google (Gmail / Calendar) object
//! normalization.

use engine_core::ids::IdError;
use serde_json::Value;

use crate::error::GoogleError;

/// A required string field, or a protocol error naming it.
pub(crate) fn req_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, GoogleError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| GoogleError::protocol(format!("missing string field {key:?}")))
}

/// An optional string field (absent for JSON `null` or a missing key).
pub(crate) fn opt_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Wraps an id-construction result, naming the field on failure.
pub(crate) fn wrap_id<T>(result: Result<T, IdError>, what: &str) -> Result<T, GoogleError> {
    result.map_err(|e| GoogleError::protocol(format!("bad {what}: {e}")))
}
