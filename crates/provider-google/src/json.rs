//! Shared JSON-extraction helpers for Google (Gmail / Calendar) object
//! normalization.

use engine_core::{ids::IdError, time::UtcDateTime};
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

/// A boolean field, defaulting to `false` when absent or non-boolean.
pub(crate) fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Parses an RFC 3339 instant field (Google's `created`/`updated`, e.g.
/// `2026-07-18T14:40:25.000Z`), tolerating fractional seconds, or `None` for absent/null.
pub(crate) fn datetime(value: &Value, key: &str) -> Result<Option<UtcDateTime>, GoogleError> {
    let Some(raw) = opt_str(value, key) else {
        return Ok(None);
    };
    // Drop any fractional-seconds component (`.NNN`) the engine's parser does not accept.
    let cleaned = strip_fractional(raw);
    UtcDateTime::parse_rfc3339(&cleaned)
        .map(Some)
        .map_err(|e| GoogleError::protocol(format!("bad {key} datetime {raw:?}: {e}")))
}

/// Removes a `.NNN` fractional-seconds run (between the seconds and the zone designator).
fn strip_fractional(s: &str) -> String {
    match s.find('.') {
        Some(dot) => {
            let rest = &s[dot + 1..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .map_or(s.len(), |i| dot + 1 + i);
            format!("{}{}", &s[..dot], &s[end..])
        }
        None => s.to_owned(),
    }
}

/// Wraps an id-construction result, naming the field on failure.
pub(crate) fn wrap_id<T>(result: Result<T, IdError>, what: &str) -> Result<T, GoogleError> {
    result.map_err(|e| GoogleError::protocol(format!("bad {what}: {e}")))
}
