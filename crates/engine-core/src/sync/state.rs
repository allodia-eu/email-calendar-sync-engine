//! Sync state cursors.

use serde::{Deserialize, Serialize};

/// An opaque, provider-specific sync cursor for one [`super::SyncScope`].
///
/// This wraps a JMAP state string, an IMAP `UIDVALIDITY`/`UIDNEXT`/`MODSEQ`
/// summary, or a CalDAV sync-token. The engine **never parses or orders** it: it
/// only stores it and compares it for equality, then round-trips it back to the
/// provider (`providers.md`, `store-and-sync.md`). Deliberately no `Ord` —
/// cursors are not monotonic in any way the engine may assume.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyncState(Box<str>);

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
    fn roundtrips_transparently_through_json() {
        let state = SyncState::new("12:0:HIGHESTMODSEQ=42");
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"12:0:HIGHESTMODSEQ=42\"");
        assert_eq!(serde_json::from_str::<SyncState>(&json).unwrap(), state);
    }
}
