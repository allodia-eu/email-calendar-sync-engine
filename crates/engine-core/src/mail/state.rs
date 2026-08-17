//! A message's mutable half, and a change to it.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{Keyword, ThreadRef};
use crate::{
    ids::{MailboxId, ProviderKey},
    membership::Memberships,
    time::UtcDateTime,
    version::RevisionTokens,
};

/// The per-message state a provider or the engine can change **without the message's bytes
/// changing**.
///
/// A message's content is immutable once the server holds it: its headers, its MIME tree and
/// its body never move. Editing a draft is not a counterexample — it mints a *new* provider
/// object on every protocol we speak (JMAP `Email` objects are immutable, so `Email/set` creates
/// one; IMAP does APPEND + EXPUNGE and the UID changes). Everything about a message that *does*
/// move is here.
///
/// The split is what decides where a field is stored:
/// [`MailContent`](super::MailContent) is the normalized payload, and this is the `message`
/// row's columns plus its `membership` rows. One home per fact, so there is no second copy to
/// go stale.
///
/// This is the **change** side — what a provider reports moved. Its read counterpart is
/// [`StoredState`], where every axis is known because the store holds it.
///
/// **Adding an axis is adding a field here**, not a new mechanism: Microsoft Graph's
/// `categories` is the next one, and it needs this field plus a `membership` kind to store it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailState {
    /// The keywords applied to the message — the RFC 8621 system ones (`$seen`, `$flagged`,
    /// `$draft`, `$answered`) and any user keyword or provider label.
    pub keywords: BTreeSet<Keyword>,
    /// The mailboxes the message is filed in, when the provider files **in place**.
    ///
    /// `None` does not mean "unchanged" — it means the protocol does not express filing as
    /// mutable state *within a scope*, because the scope carries it. An IMAP move mints a new
    /// UID and so a new key. A Graph move keeps the message's immutable id, but the adapter is
    /// folder-bound: the message leaves the source folder's scope (a `@removed` in its delta)
    /// and appears in the destination folder's, so what moved is which scope holds it, not a
    /// membership inside one. Neither provider can report a filing change against a key the
    /// scope keeps, so neither has anything to say here and the store leaves the mailbox
    /// memberships alone.
    ///
    /// `Some` is the **complete resulting** set, as JMAP and Gmail report it — both of which do
    /// move a message in place, and for both of which a "keyword change" and a "move" are the
    /// same kind of event (RFC 8621 makes `keywords` and `mailboxIds` the only mutable `Email`
    /// properties; a Gmail archive is the `INBOX` label being removed).
    pub mailboxes: Option<Memberships<MailboxId>>,
    /// The revision tokens a conditional write quotes.
    ///
    /// State, not content: an IMAP `MODSEQ` bumps on a *flag* change and a Graph `ChangeKey`
    /// bumps on an `isRead` edit, so a copy kept beside the immutable half would go stale the
    /// moment a state-only change landed — and a stale token quoted in an `If-Match` is a
    /// spurious `412` that reads like a server fault.
    ///
    /// `RevisionTokens::schedule_tag` is CalDAV scheduling state and is never set on a message.
    pub revisions: RevisionTokens,
    /// When the provider last changed the object — which is to say, when this state last moved.
    pub last_modified: Option<UtcDateTime>,
}

impl MailState {
    /// The state of a message carrying `keywords`, whose provider does not file in place.
    #[must_use]
    pub fn with_keywords(keywords: BTreeSet<Keyword>) -> Self {
        Self {
            keywords,
            ..Self::default()
        }
    }

    /// Attaches the complete set of mailboxes the message is now filed in — for a provider that
    /// moves a message in place under a stable key.
    #[must_use]
    pub fn filed_in(mut self, mailboxes: Memberships<MailboxId>) -> Self {
        self.mailboxes = Some(mailboxes);
        self
    }

    /// Attaches the revision tokens and modification time the provider reported with this state.
    #[must_use]
    pub fn revised(
        mut self,
        revisions: RevisionTokens,
        last_modified: Option<UtcDateTime>,
    ) -> Self {
        self.revisions = revisions;
        self.last_modified = last_modified;
        self
    }
}

/// A message's mutable half as the **store** holds it — the read counterpart of [`MailState`].
///
/// Every axis is definite here, because the store knows all of them: the message row carries the
/// flags, the derived thread and the revision tokens, and the `membership` junction carries the
/// keywords and the filing. [`MailState`]'s optional `mailboxes` models a *provider's* silence
/// on an axis; the store is never silent.
///
/// This is half of what [`Message::from_parts`](super::Message::from_parts) needs; the other
/// half is the stored payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredState {
    /// The mailboxes the message is filed in, read from the `membership` junction.
    pub mailboxes: Memberships<MailboxId>,
    /// Every keyword on the message.
    pub keywords: BTreeSet<Keyword>,
    /// The thread the **engine derived** for this message, if it derived one. A
    /// provider-assigned thread rides the payload instead, and wins.
    pub thread: Option<ThreadRef>,
    /// The revision tokens a conditional write quotes.
    pub revisions: RevisionTokens,
    /// When the provider last changed the object.
    pub last_modified: Option<UtcDateTime>,
}

/// A message whose provider reported a change to its [`MailState`] and nothing else.
///
/// Every adapter can recognise one: an IMAP `CHANGEDSINCE` row carrying `FLAGS` with no
/// `ENVELOPE`, a Gmail `labelsAdded`/`labelsRemoved` history record, a Microsoft Graph delta
/// entry without an `@odata.etag`, a JMAP id in `Email/changes`'s `updated` rather than its
/// `created`. Each then costs one small call to read the resulting state — or none at all, where
/// the delta already carried it — instead of re-fetching a whole message.
///
/// It carries the **complete resulting state**, never a delta. A complete state is idempotent on
/// replay, which an add/remove pair is not: a sync that re-delivers a page must leave the same
/// result. Every provider can supply it in one cheap call — IMAP's `FLAGS` is already the whole
/// set, JMAP asks `Email/get` for two properties, Gmail's history record carries it inline, Graph
/// a narrow `$select` — so nothing is bought by carrying the narrower form.
///
/// An adapter that cannot tell a state change from a content change emits none of these; its
/// messages ride in a page's `changed` as whole objects, exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailStateChange {
    /// The message whose state moved.
    pub key: ProviderKey,
    /// The state **after** the change.
    pub state: MailState,
}

impl MailStateChange {
    /// A state change for `key` resulting in `state`.
    #[must_use]
    pub fn new(key: ProviderKey, state: MailState) -> Self {
        Self { key, state }
    }

    /// A state change whose only moved axis is the keyword set — the shape a mark-read takes on a
    /// provider that files through identity (IMAP, Graph).
    #[must_use]
    pub fn keywords(key: ProviderKey, keywords: BTreeSet<Keyword>) -> Self {
        Self::new(key, MailState::with_keywords(keywords))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::SystemKeyword;

    #[test]
    fn roundtrips_through_json() {
        let change = MailStateChange::keywords(
            ProviderKey::new("inbox/1/42").unwrap(),
            [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
        );
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(
            serde_json::from_str::<MailStateChange>(&json).unwrap(),
            change
        );
    }

    #[test]
    fn an_empty_state_is_a_real_change() {
        // Clearing the last keyword — marking a read message unread — is the case a "skip it if
        // there is nothing to write" shortcut would silently drop.
        let change =
            MailStateChange::keywords(ProviderKey::new("inbox/1/42").unwrap(), BTreeSet::new());
        assert!(change.state.keywords.is_empty());
    }

    #[test]
    fn filing_is_absent_until_a_provider_that_files_in_place_reports_it() {
        // The distinction the store reads: absent means "this protocol cannot move a message
        // under a stable key", so the mailbox memberships are not this change's to touch.
        let change =
            MailStateChange::keywords(ProviderKey::new("inbox/1/42").unwrap(), BTreeSet::new());
        assert!(change.state.mailboxes.is_none());

        let moved = MailState::with_keywords(BTreeSet::new())
            .filed_in(Memberships::of_one(MailboxId::try_from("Archive").unwrap()));
        assert!(moved.mailboxes.is_some());
    }
}
