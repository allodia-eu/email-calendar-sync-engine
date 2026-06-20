//! The per-mailbox IMAP sync cursor and the paging token, both opaque to the
//! engine.
//!
//! IMAP sync state is per mailbox: the `(UIDVALIDITY, UIDNEXT)` pair (RFC 9051
//! §2.3.1). A change in `UIDVALIDITY` means the server renumbered the UID space, so
//! every prior key is invalid and the next pass must be a snapshot (rediscovery) —
//! the [`crate::sync`] layer reads that off the decoded cursor. `HIGHESTMODSEQ`
//! (CONDSTORE) is a deferred capability and not carried here.

use engine_core::sync::SyncState;
use engine_provider::PageToken;

/// The decoded per-mailbox cursor: the UID space identity and the next-UID
/// watermark new arrivals are fetched above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MailboxCursor {
    /// `UIDVALIDITY` — a change invalidates every prior key.
    pub uid_validity: u32,
    /// `UIDNEXT` at the end of the last pass — the delta watermark.
    pub uid_next: u32,
}

impl MailboxCursor {
    /// Encodes the cursor as an opaque [`SyncState`] (`v<validity>;n<next>`).
    pub(crate) fn encode(self) -> SyncState {
        SyncState::new(format!("v{};n{}", self.uid_validity, self.uid_next))
    }

    /// Decodes a [`SyncState`] this adapter wrote; `None` for any other shape
    /// (treated as "no prior cursor" → snapshot).
    pub(crate) fn decode(state: &SyncState) -> Option<Self> {
        let (validity, next) = state.as_str().strip_prefix('v')?.split_once(";n")?;
        Some(Self {
            uid_validity: validity.parse().ok()?,
            uid_next: next.parse().ok()?,
        })
    }
}

/// Encodes the next page's high UID boundary into an opaque [`PageToken`]. The next
/// page fetches the UID window ending at this boundary.
pub(crate) fn page_token(next_high: u32) -> PageToken {
    PageToken::new(next_high.to_string())
}

/// Decodes a [`PageToken`] this adapter wrote back into its high UID boundary.
pub(crate) fn page_high(token: &PageToken) -> Option<u32> {
    token.as_str().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrips_through_an_opaque_state() {
        let cursor = MailboxCursor {
            uid_validity: 1_234_567_890,
            uid_next: 42,
        };
        let state = cursor.encode();
        assert_eq!(state.as_str(), "v1234567890;n42");
        assert_eq!(MailboxCursor::decode(&state), Some(cursor));
    }

    #[test]
    fn a_foreign_or_garbage_state_decodes_to_none() {
        // A JMAP-style state string is not ours → treated as no cursor (snapshot).
        assert_eq!(MailboxCursor::decode(&SyncState::new("jmap-state-7")), None);
        assert_eq!(MailboxCursor::decode(&SyncState::new("v1")), None);
        assert_eq!(MailboxCursor::decode(&SyncState::new("vx;ny")), None);
    }

    #[test]
    fn page_token_roundtrips_its_boundary() {
        let token = page_token(99);
        assert_eq!(token.as_str(), "99");
        assert_eq!(page_high(&token), Some(99));
        assert_eq!(page_high(&PageToken::new("not-a-number")), None);
    }
}
