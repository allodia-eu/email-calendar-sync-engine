//! The per-mailbox IMAP sync cursor and the paging token, both opaque to the
//! engine.
//!
//! IMAP sync state is per mailbox: the `(UIDVALIDITY, UIDNEXT)` pair (RFC 9051
//! §2.3.1), plus an optional `HIGHESTMODSEQ` (CONDSTORE/QRESYNC, RFC 7162) when the
//! server advertises QRESYNC. A change in `UIDVALIDITY` means the server renumbered
//! the UID space, so every prior key is invalid and the next pass must be a snapshot
//! (rediscovery) — the [`crate::sync`] layer reads that off the decoded cursor. The
//! `HIGHESTMODSEQ`, when present, is the baseline a QRESYNC delta passes to
//! `CHANGEDSINCE`/`VANISHED` to reconcile flag changes and expunges incrementally
//! ([`crate::qresync`]); a cursor written before QRESYNC support (no `;m`) decodes
//! with `None`, so the first delta after an upgrade is a plain new-arrivals delta
//! that then records the modseq.

use engine_core::sync::SyncState;
use engine_provider::PageToken;

/// The decoded per-mailbox cursor: the UID space identity, the next-UID watermark
/// new arrivals are fetched above, and the `HIGHESTMODSEQ` baseline for a QRESYNC
/// delta (absent on a non-QRESYNC session or a pre-QRESYNC cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MailboxCursor {
    /// `UIDVALIDITY` — a change invalidates every prior key.
    pub uid_validity: u32,
    /// `UIDNEXT` at the end of the last pass — the delta watermark.
    pub uid_next: u32,
    /// `HIGHESTMODSEQ` at the end of the last pass (RFC 7162) — the `CHANGEDSINCE`
    /// baseline a QRESYNC delta reconciles flag/expunge changes against. `None` when
    /// the session has no QRESYNC or the cursor predates QRESYNC support.
    pub highest_modseq: Option<u64>,
}

impl MailboxCursor {
    /// Encodes the cursor as an opaque [`SyncState`] (`v<validity>;n<next>`, plus
    /// `;m<modseq>` when a `HIGHESTMODSEQ` is carried). Omitting `;m` keeps a
    /// non-QRESYNC cursor byte-identical to the pre-QRESYNC format.
    pub(crate) fn encode(self) -> SyncState {
        match self.highest_modseq {
            Some(modseq) => SyncState::new(format!(
                "v{};n{};m{}",
                self.uid_validity, self.uid_next, modseq
            )),
            None => SyncState::new(format!("v{};n{}", self.uid_validity, self.uid_next)),
        }
    }

    /// Decodes a [`SyncState`] this adapter wrote; `None` for any other shape
    /// (treated as "no prior cursor" → snapshot). A cursor with no `;m` suffix (one
    /// written before QRESYNC support) decodes with `highest_modseq: None`.
    pub(crate) fn decode(state: &SyncState) -> Option<Self> {
        let rest = state.as_str().strip_prefix('v')?;
        let (validity, rest) = rest.split_once(";n")?;
        let (next, highest_modseq) = match rest.split_once(";m") {
            Some((next, modseq)) => (next, Some(modseq.parse().ok()?)),
            None => (rest, None),
        };
        Some(Self {
            uid_validity: validity.parse().ok()?,
            uid_next: next.parse().ok()?,
            highest_modseq,
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
            highest_modseq: None,
        };
        let state = cursor.encode();
        assert_eq!(state.as_str(), "v1234567890;n42");
        assert_eq!(MailboxCursor::decode(&state), Some(cursor));
    }

    #[test]
    fn a_qresync_cursor_roundtrips_with_its_modseq() {
        let cursor = MailboxCursor {
            uid_validity: 1000,
            uid_next: 42,
            highest_modseq: Some(9_223_372_036_854_775_807), // a u63 MODSEQ ceiling
        };
        let state = cursor.encode();
        assert_eq!(state.as_str(), "v1000;n42;m9223372036854775807");
        assert_eq!(MailboxCursor::decode(&state), Some(cursor));
    }

    #[test]
    fn a_pre_qresync_cursor_decodes_with_no_modseq() {
        // A cursor written before QRESYNC support has no `;m`; it must still decode,
        // with `highest_modseq: None`, so the upgrade is seamless.
        let decoded = MailboxCursor::decode(&SyncState::new("v1000;n42")).unwrap();
        assert_eq!(decoded.uid_next, 42);
        assert_eq!(decoded.highest_modseq, None);
    }

    #[test]
    fn a_foreign_or_garbage_state_decodes_to_none() {
        // A JMAP-style state string is not ours → treated as no cursor (snapshot).
        assert_eq!(MailboxCursor::decode(&SyncState::new("jmap-state-7")), None);
        assert_eq!(MailboxCursor::decode(&SyncState::new("v1")), None);
        assert_eq!(MailboxCursor::decode(&SyncState::new("vx;ny")), None);
        // A non-numeric modseq is garbage, not "no modseq".
        assert_eq!(MailboxCursor::decode(&SyncState::new("v1000;n42;mx")), None);
    }

    #[test]
    fn page_token_roundtrips_its_boundary() {
        let token = page_token(99);
        assert_eq!(token.as_str(), "99");
        assert_eq!(page_high(&token), Some(99));
        assert_eq!(page_high(&PageToken::new("not-a-number")), None);
    }
}
