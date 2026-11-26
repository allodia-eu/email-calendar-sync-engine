//! Sync state cursors.

use serde::{Deserialize, Serialize};

/// An opaque, provider-specific sync cursor for one [`super::SyncScope`].
///
/// This wraps a JMAP state string, an IMAP `UIDVALIDITY`/`UIDNEXT`/`MODSEQ`
/// summary, or a CalDAV sync-token. The engine **never parses or orders** it: it
/// only stores it and compares it for equality, then round-trips it back to the
/// provider (`providers.md`, `store-and-sync.md`). Deliberately no `Ord` —
/// cursors are not monotonic in any way the engine may assume.
///
/// A cursor can carry content (Graph keeps its whole deltaLink URL here, whose path names the
/// mailbox's address), so its `Debug` shows the value only where
/// [`crate::redact::SHOWS_VALUES`] holds; [`as_str`](Self::as_str) always does.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyncState(Box<str>);

impl core::fmt::Debug for SyncState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("SyncState")
            .field(&crate::redact::Content(&self.0))
            .finish()
    }
}

impl SyncState {
    /// Wraps a provider cursor string.
    #[must_use]
    pub fn new(cursor: impl Into<String>) -> Self {
        Self(cursor.into().into_boxed_str())
    }

    /// Returns the opaque cursor as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_opaque_and_compared_by_equality() {
        let a = SyncState::new("state-abc");
        let b = SyncState::new("state-abc");
        let c = SyncState::new("state-xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_str(), "state-abc");
    }

    #[test]
    fn debug_shows_the_cursor_only_where_the_build_shows_values() {
        // Graph keeps a whole deltaLink here, and its path names the mailbox's address.
        let state = SyncState::new("https://graph.example/users/a@example.com/delta");
        let expected = if crate::redact::SHOWS_VALUES {
            r#"SyncState("https://graph.example/users/a@example.com/delta")"#
        } else {
            "SyncState(<redacted, 47 bytes>)"
        };
        assert_eq!(format!("{state:?}"), expected);
    }

    #[test]
    fn roundtrips_transparently_through_json() {
        let state = SyncState::new("12:0:HIGHESTMODSEQ=42");
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"12:0:HIGHESTMODSEQ=42\"");
        assert_eq!(serde_json::from_str::<SyncState>(&json).unwrap(), state);
    }
}
