//! Paged scope sync — the streaming primitive.
//!
//! A fetch that spans many pages is the natural shape of every provider's list/
//! query/changes API (JMAP query position, IMAP UID ranges, Gmail/Graph page
//! tokens and delta links). [`SyncPage`] is one page of changes plus how to
//! continue; the orchestrator commits each page additively for a responsive UI,
//! reports progress, and persists the cursor only once the pass completes.

use engine_core::{
    ids::ProviderKey,
    sync::{SyncObject, SyncState},
};

/// Whether a sync pass is a full/bounded snapshot or an incremental delta.
///
/// Consistent across every page of one pass. The orchestrator tombstones absent
/// rows at end-of-pass only for a snapshot; a delta carries removals inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    /// A full/bounded snapshot: the accumulated `present` ids drive end-of-pass
    /// tombstoning.
    Snapshot,
    /// An incremental delta: `removed` keys are applied inline; no tombstoning.
    Delta,
}

/// An opaque, provider-specific continuation **within** one sync pass.
///
/// Distinct from [`SyncState`], which resumes the *next* pass: a page token only
/// orders the current fetch. JMAP encodes a query position; IMAP a UID range;
/// Gmail/Graph a page token or delta link. The engine never parses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageToken(Box<str>);

impl PageToken {
    /// Wraps a provider continuation string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().into_boxed_str())
    }

    /// Returns the continuation as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One page of a scope's changes, plus how to continue and the cursor to persist.
///
/// `T` is the scope's normalized object type (a `Message`, …). All pages of a
/// pass share [`kind`](SyncPage::kind) and report the same [`total`](SyncPage::total);
/// [`next_cursor`](SyncPage::next_cursor) is only meaningful once the last page
/// applies.
#[derive(Debug, Clone)]
pub struct SyncPage<T: SyncObject> {
    /// Whether the whole pass is a snapshot or a delta.
    pub kind: SyncKind,
    /// Objects created or updated **in full** in this page.
    pub changed: Vec<T>,
    /// Objects this page reported a **partial** change for — only the fields the patch names
    /// moved, so the store writes those columns and leaves the rest, and the normalized
    /// payload, alone.
    ///
    /// Uninhabited for an object with no partial form, so a calendar page cannot carry one.
    pub patched: Vec<T::Patch>,
    /// Keys removed in this page (delta passes only; empty for a snapshot).
    pub removed: Vec<ProviderKey>,
    /// For a snapshot pass, the complete id set **this page covers**, so the
    /// orchestrator can accumulate the full present set and tombstone at end of
    /// pass. Empty for a delta.
    pub present: Vec<ProviderKey>,
    /// The continuation for the next page, or `None` when this is the last page.
    pub next_page: Option<PageToken>,
    /// The cursor to persist once the **whole** pass has applied (the orchestrator
    /// ignores it until the last page).
    pub next_cursor: SyncState,
    /// Total objects in the pass, if the provider can compute it (the progress
    /// denominator). Stable across pages.
    pub total: Option<usize>,
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::{MailboxId, MessageId},
        mail::Message,
        membership::Memberships,
    };

    use super::*;

    fn message(id: &str) -> Message {
        Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        )
    }

    #[test]
    fn page_token_is_opaque() {
        let token = PageToken::new("position:50");
        assert_eq!(token.as_str(), "position:50");
        assert_eq!(token, PageToken::new("position:50"));
    }

    #[test]
    fn a_single_complete_page_has_no_continuation() {
        let page: SyncPage<Message> = SyncPage {
            kind: SyncKind::Snapshot,
            changed: vec![message("a")],
            patched: vec![],
            removed: vec![],
            present: vec![ProviderKey::new("a").unwrap()],
            next_page: None,
            next_cursor: SyncState::new("s1"),
            total: Some(1),
        };
        assert!(page.next_page.is_none());
        assert_eq!(page.total, Some(1));
    }
}
