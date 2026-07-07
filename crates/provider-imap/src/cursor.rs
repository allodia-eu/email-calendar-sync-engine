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
/// new arrivals are fetched above, the `HIGHESTMODSEQ` baseline for a QRESYNC
/// delta, and — while a cold backfill is still descending — the lowest UID synced
/// so far, so a killed backfill resumes below it instead of restarting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MailboxCursor {
    /// `UIDVALIDITY` — a change invalidates every prior key.
    pub uid_validity: u32,
    /// `UIDNEXT` frontier — new arrivals are fetched at or above it. Captured at the
    /// start of a backfill (so mail arriving during the backfill is caught by the
    /// first delta afterwards) and advanced by each delta.
    pub uid_next: u32,
    /// `HIGHESTMODSEQ` at the end of the last pass (RFC 7162) — the `CHANGEDSINCE`
    /// baseline a QRESYNC delta reconciles flag/expunge changes against. `None` when
    /// the session has no QRESYNC or the cursor predates QRESYNC support.
    pub highest_modseq: Option<u64>,
    /// **Backfill watermark:** the lowest UID committed so far by an in-progress cold
    /// backfill. `Some(low)` means the backfill is still descending and the next pass
    /// resumes below `low`; `None` means the backfill is complete (steady state), so a
    /// pass is a delta above [`uid_next`](Self::uid_next). This is what makes a cold
    /// sync killed mid-flight resume from where it stopped (`store-and-sync.md`).
    pub backfill_low: Option<u32>,
}

impl MailboxCursor {
    /// Encodes the cursor as an opaque [`SyncState`]: `v<validity>;n<next>`, then an
    /// optional `;m<modseq>` (QRESYNC baseline) and an optional `;b<low>` (in-progress
    /// backfill watermark). Omitting both keeps a completed non-QRESYNC cursor
    /// byte-identical to the pre-backfill format.
    pub(crate) fn encode(self) -> SyncState {
        use core::fmt::Write as _;

        let mut s = format!("v{};n{}", self.uid_validity, self.uid_next);
        if let Some(modseq) = self.highest_modseq {
            let _ = write!(s, ";m{modseq}");
        }
        if let Some(low) = self.backfill_low {
            let _ = write!(s, ";b{low}");
        }
        SyncState::new(s)
    }

    /// Decodes a [`SyncState`] this adapter wrote; `None` for any other shape
    /// (treated as "no prior cursor" → snapshot). Suffixes `;m` and `;b` are each
    /// optional and independent, so a pre-backfill cursor still decodes.
    pub(crate) fn decode(state: &SyncState) -> Option<Self> {
        let rest = state.as_str().strip_prefix('v')?;
        let (validity, rest) = rest.split_once(";n")?;
        // Split off `;b<low>` first (it is always the last suffix), then `;m<modseq>`.
        let (rest, backfill_low) = match rest.split_once(";b") {
            Some((head, low)) => (head, Some(low.parse().ok()?)),
            None => (rest, None),
        };
        let (next, highest_modseq) = match rest.split_once(";m") {
            Some((next, modseq)) => (next, Some(modseq.parse().ok()?)),
            None => (rest, None),
        };
        Some(Self {
            uid_validity: validity.parse().ok()?,
            uid_next: next.parse().ok()?,
            highest_modseq,
            backfill_low,
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
            backfill_low: None,
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
            backfill_low: None,
        };
        let state = cursor.encode();
        assert_eq!(state.as_str(), "v1000;n42;m9223372036854775807");
        assert_eq!(MailboxCursor::decode(&state), Some(cursor));
    }

    #[test]
    fn a_backfill_cursor_roundtrips_with_its_watermark() {
        // A mid-backfill cursor carries the lowest UID synced so far, with and without
        // a QRESYNC modseq; a resume reads it to continue below the watermark.
        let cursor = MailboxCursor {
            uid_validity: 1000,
            uid_next: 500,
            highest_modseq: Some(77),
            backfill_low: Some(120),
        };
        let state = cursor.encode();
        assert_eq!(state.as_str(), "v1000;n500;m77;b120");
        assert_eq!(MailboxCursor::decode(&state), Some(cursor));

        let no_modseq = MailboxCursor {
            uid_validity: 1000,
            uid_next: 500,
            highest_modseq: None,
            backfill_low: Some(120),
        };
        let state = no_modseq.encode();
        assert_eq!(state.as_str(), "v1000;n500;b120");
        assert_eq!(MailboxCursor::decode(&state), Some(no_modseq));
    }

    #[test]
    fn a_pre_qresync_cursor_decodes_with_no_modseq() {
        // A cursor written before QRESYNC support has no `;m`; it must still decode,
        // with `highest_modseq: None`, so the upgrade is seamless.
        let decoded = MailboxCursor::decode(&SyncState::new("v1000;n42")).unwrap();
        assert_eq!(decoded.uid_next, 42);
        assert_eq!(decoded.highest_modseq, None);
        assert_eq!(decoded.backfill_low, None);
    }

    #[test]
    fn a_foreign_or_garbage_state_decodes_to_none() {
        // A JMAP-style state string is not ours → treated as no cursor (snapshot).
        assert_eq!(MailboxCursor::decode(&SyncState::new("jmap-state-7")), None);
        assert_eq!(MailboxCursor::decode(&SyncState::new("v1")), None);
        assert_eq!(MailboxCursor::decode(&SyncState::new("vx;ny")), None);
        // A non-numeric modseq is garbage, not "no modseq".
        assert_eq!(MailboxCursor::decode(&SyncState::new("v1000;n42;mx")), None);
        // A non-numeric backfill watermark is likewise garbage.
        assert_eq!(MailboxCursor::decode(&SyncState::new("v1000;n42;bx")), None);
    }

    #[test]
    fn page_token_roundtrips_its_boundary() {
        let token = page_token(99);
        assert_eq!(token.as_str(), "99");
        assert_eq!(page_high(&token), Some(99));
        assert_eq!(page_high(&PageToken::new("not-a-number")), None);
    }
}
