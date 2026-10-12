//! Pure parsing of JMAP `/get`, `/query`, and `/changes` results into the
//! engine's sync shapes.
//!
//! These functions take a method result `Value` and return objects, id sets,
//! cursors, or a [`Changes`] diff — no I/O — so the sync orchestration in
//! [`crate::provider`] stays a thin "build request → execute → parse" shell and
//! the parsing is unit-tested offline (RFC 8620 §5.1 `/get`, §5.2 `/changes`,
//! §5.5 `/query`).

use engine_core::{ids::ProviderKey, sync::SyncState};
use serde_json::Value;

use crate::error::JmapError;

/// The created/updated/destroyed diff a `Foo/changes` call returns (RFC 8620
/// §5.2), with the cursor to advance to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Changes {
    /// Ids created since the prior state.
    pub(crate) created: Vec<ProviderKey>,
    /// Ids updated since the prior state.
    pub(crate) updated: Vec<ProviderKey>,
    /// Ids destroyed since the prior state.
    pub(crate) destroyed: Vec<ProviderKey>,
    /// The cursor to advance to.
    pub(crate) new_state: SyncState,
    /// Whether the server has more changes beyond this response.
    pub(crate) has_more: bool,
}

impl Changes {
    /// Parses a `Foo/changes` result.
    pub(crate) fn parse(result: &Value) -> Result<Self, JmapError> {
        Ok(Self {
            created: keys(result, "created")?,
            updated: keys(result, "updated")?,
            destroyed: keys(result, "destroyed")?,
            new_state: state(result, "newState")?,
            has_more: result
                .get("hasMoreChanges")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Normalizes every object in a `/get` result's `list` with `normalize`.
pub(crate) fn objects<T>(
    result: &Value,
    normalize: impl Fn(&Value) -> Result<T, JmapError>,
) -> Result<Vec<T>, JmapError> {
    let list = result
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| JmapError::protocol("/get result has no list"))?;
    list.iter().map(normalize).collect()
}

/// Reads a string-array field as an ordered list of [`ProviderKey`]s.
pub(crate) fn keys(result: &Value, field: &str) -> Result<Vec<ProviderKey>, JmapError> {
    let Some(list) = result.get(field).and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    list.iter()
        .map(|id| {
            let id = id
                .as_str()
                .ok_or_else(|| JmapError::protocol(format!("{field} entry is not a string")))?;
            ProviderKey::new(id).map_err(|e| JmapError::protocol(format!("bad id {id:?}: {e}")))
        })
        .collect()
}

/// Reads a required state-cursor field (`state` for `/get`, `newState` for
/// `/changes`, `queryState` for `/query`).
pub(crate) fn state(result: &Value, field: &str) -> Result<SyncState, JmapError> {
    result
        .get(field)
        .and_then(Value::as_str)
        .map(SyncState::new)
        .ok_or_else(|| JmapError::protocol(format!("missing {field} cursor")))
}

/// Reads a `Foo/query` `total` count (present only with `calculateTotal`), or
/// `None` if the server did not report one.
pub(crate) fn total(result: &Value) -> Option<usize> {
    result
        .get("total")
        .and_then(Value::as_u64)
        .and_then(|t| usize::try_from(t).ok())
}

/// Clamps a requested page size to the server's `maxObjectsInGet` (RFC 8620 core
/// capability), treating a requested `0` as "as many as the server allows".
pub(crate) fn clamp_limit(requested: usize, max_objects_in_get: usize) -> usize {
    if requested == 0 {
        max_objects_in_get
    } else {
        requested.min(max_objects_in_get)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mail::message_from_json;

    const EMAIL_GET: &str = include_str!("../tests/fixtures/email_get.json");

    #[test]
    fn objects_normalizes_the_get_list() {
        let result: Value = serde_json::from_str(EMAIL_GET).unwrap();
        let messages = objects(&result, message_from_json).unwrap();
        assert_eq!(messages.len(), 9);
    }

    #[test]
    fn get_state_is_required() {
        let result: Value = serde_json::from_str(EMAIL_GET).unwrap();
        assert_eq!(state(&result, "state").unwrap().as_str(), "sb2");
        assert!(state(&json!({}), "state").is_err());
    }

    #[test]
    fn changes_parses_created_updated_destroyed_and_cursor() {
        let result = json!({
            "accountId": "c",
            "oldState": "s1",
            "newState": "s2",
            "hasMoreChanges": false,
            "created": ["e1", "e2"],
            "updated": ["e3"],
            "destroyed": ["e4"]
        });
        let changes = Changes::parse(&result).unwrap();
        assert_eq!(changes.created.len(), 2);
        assert_eq!(changes.updated.len(), 1);
        assert_eq!(changes.destroyed.len(), 1);
        assert_eq!(changes.new_state.as_str(), "s2");
        assert!(!changes.has_more);
    }

    #[test]
    fn empty_changes_is_a_valid_no_op_delta() {
        let result = json!({ "newState": "s1", "created": [], "updated": [], "destroyed": [] });
        let changes = Changes::parse(&result).unwrap();
        assert!(changes.created.is_empty());
        assert!(changes.updated.is_empty());
        assert_eq!(changes.new_state.as_str(), "s1");
    }

    #[test]
    fn a_query_state_reads_as_a_cursor() {
        let query = json!({ "ids": ["a", "b"], "queryState": "q1" });
        assert_eq!(state(&query, "queryState").unwrap().as_str(), "q1");
    }

    #[test]
    fn malformed_id_arrays_error_rather_than_panic() {
        assert!(keys(&json!({ "created": [42] }), "created").is_err());
        // Absent array is an empty list, not an error.
        assert!(keys(&json!({}), "created").unwrap().is_empty());
    }

    #[test]
    fn clamp_limit_respects_the_server_ceiling_and_treats_zero_as_max() {
        assert_eq!(clamp_limit(50, 500), 50);
        assert_eq!(clamp_limit(900, 500), 500); // above the ceiling → clamped
        assert_eq!(clamp_limit(0, 500), 500); // 0 means "as many as allowed"
    }
}
