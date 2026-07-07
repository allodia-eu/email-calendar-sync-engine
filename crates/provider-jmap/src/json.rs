//! Shared JSON-extraction helpers for JMAP object normalization.

use engine_core::{ids::IdError, time::UtcDateTime};
use serde_json::Value;

use crate::error::JmapError;

/// A required string field, or a protocol error naming it.
pub(crate) fn req_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, JmapError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| JmapError::protocol(format!("missing string field {key:?}")))
}

/// An optional string field (absent for JSON `null` or a missing key).
pub(crate) fn opt_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The keys of a `{ id: true }` set object (`mailboxIds`, `keywords`,
/// `calendarIds`, `roles`), in JSON order, keeping only `true` entries.
pub(crate) fn true_keys<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a str> {
    value
        .get(key)
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| {
            map.iter()
                .filter(|(_, set)| set.as_bool() == Some(true))
                .map(|(name, _)| name.as_str())
        })
}

/// Parses a `Z`-terminated `UTCDate` field (RFC 8620 §1.4 — `receivedAt`,
/// `created`, `updated`), or `None` for an absent/null value.
pub(crate) fn datetime(value: &Value, key: &str) -> Result<Option<UtcDateTime>, JmapError> {
    match value.get(key).and_then(Value::as_str) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<UtcDateTime>()
            .map(Some)
            .map_err(|e| JmapError::protocol(format!("bad {key} datetime {raw:?}: {e}"))),
    }
}

/// Parses the JMAP `sentAt` field (the message `Date` header, RFC 8621 §4.1.1).
///
/// Unlike [`datetime`], `sentAt` is a JMAP `Date` (RFC 8620 §1.4), not a
/// `UTCDate`: it legitimately carries a numeric UTC offset (Fastmail and other
/// servers emit the sender's local offset), so it is parsed through the fuller
/// RFC 3339 grammar and normalized to UTC. A malformed value degrades to `None`
/// rather than failing — one unparseable optional field must never abort the sync
/// of the rest of the mailbox.
pub(crate) fn sent_at(value: &Value) -> Option<UtcDateTime> {
    value
        .get("sentAt")
        .and_then(Value::as_str)
        .and_then(|raw| UtcDateTime::parse_rfc3339(raw).ok())
}

/// Wraps an id-construction result, naming the field on failure.
pub(crate) fn wrap_id<T>(result: Result<T, IdError>, what: &str) -> Result<T, JmapError> {
    result.map_err(|e| JmapError::protocol(format!("bad {what}: {e}")))
}
