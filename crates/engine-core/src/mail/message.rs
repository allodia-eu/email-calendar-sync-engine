//! The normalized mail object.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{EmailBodyPart, Envelope, Keyword, SystemKeyword, ThreadRef};
use crate::{
    attachment::Attachment,
    extended::ExtendedProperties,
    ids::{BlobId, MailboxId, MessageId, ThreadId},
    membership::Memberships,
    time::UtcDateTime,
    version::RevisionTokens,
};

/// A stored mail object — a *provider* object, not a deduplicated RFC 5322
/// message.
///
/// Identity is [`MessageId`] (opaque, provider-assigned; the IMAP adapter
/// synthesizes a stable key from `(mailbox, UIDVALIDITY, UID)`). Membership in
/// mailboxes/labels is a separate, non-empty set: a JMAP/Gmail object carries
/// several memberships, while two IMAP copies in different folders are *distinct*
/// `Message`s each with a single membership. Keywords are the per-object state
/// axis. Timestamps are kept separately — `received_at` (the internal delivery
/// date), `sent_at` (the `Date` header instant), and `last_modified` — as
/// required by `modeling.md`.
///
/// UI/search deduplication across copies is presentation policy, applied above
/// this type; it never collapses two provider objects into one here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// The provider object id.
    pub id: MessageId,
    /// The blob holding the raw RFC 5322 bytes; `None` until known. Not stable
    /// across writes (RFC 8620 §6).
    pub blob_id: Option<BlobId>,
    /// The thread this message belongs to, if threading is resolved, and whether
    /// the provider assigned that id or the engine derived it.
    ///
    /// **Not carried by the stored payload** ([`MailContent`]): a thread id is mutable state the
    /// message row owns, so a payload that carried one could disagree with it. Absent when
    /// decoded straight from storage, and filled by whatever composes a `Message` back out of
    /// the row.
    #[serde(default)]
    pub thread: Option<ThreadRef>,
    /// The mailboxes/labels this message belongs to (always at least one).
    pub mailboxes: Memberships<MailboxId>,
    /// The keywords applied to this message.
    ///
    /// **Not carried by the stored payload** ([`MailContent`]), for the same reason as
    /// [`thread`](Message::thread): keywords move without the provider ever re-sending the
    /// object, and their home is the message row plus the membership junction.
    #[serde(default)]
    pub keywords: BTreeSet<Keyword>,
    /// The size of the raw message in octets, if known.
    pub size: Option<u64>,
    /// The delivery/internal date (IMAP internal date, JMAP `receivedAt`).
    pub received_at: Option<UtcDateTime>,
    /// The instant from the `Date` header (JMAP `sentAt`), normalized to UTC.
    pub sent_at: Option<UtcDateTime>,
    /// When the object was last modified at the provider.
    ///
    /// **Not carried by the stored payload** — it moves when the message's *state* moves, so its
    /// home is the message row ([`MailState`](super::MailState)).
    #[serde(default)]
    pub last_modified: Option<UtcDateTime>,
    /// The parsed addressing/threading headers.
    pub envelope: Envelope,
    /// A short snippet for list views (≤256 characters; JMAP `preview`).
    pub preview: Option<String>,
    /// Whether the message has a non-inline attachment (server-set heuristic).
    pub has_attachment: bool,
    /// The full normalized MIME tree, when synced (absent at metadata-only
    /// tiers).
    pub mime_structure: Option<EmailBodyPart>,
    /// The normalized attachments.
    pub attachments: Vec<Attachment>,
    /// The reply-unique body text (the part unique to this message), used for
    /// snippets and indexing, when available (e.g. Graph `uniqueBody`).
    pub reply_unique_text: Option<String>,
    /// Per-object revision tokens, if the provider supplies any.
    ///
    /// **Not carried by the stored payload**: an IMAP `MODSEQ` bumps on a flag change and a
    /// Graph `ChangeKey` on an `isRead` edit, so these move with the message's state, not with
    /// its bytes. Their home is the message row ([`MailState`](super::MailState)).
    #[serde(default)]
    pub revisions: RevisionTokens,
    /// Preserved provider-defined extended properties.
    pub extended: ExtendedProperties,
}

/// The mail payload the store persists: a message's **immutable half**.
///
/// A message's content never changes once the server holds it. Editing a draft is not a
/// counterexample — it mints a *new* provider object on every protocol we speak (a JMAP `Email`
/// is immutable, IMAP does APPEND + EXPUNGE and the UID changes). So this can be written once
/// and never reconciled.
///
/// Its mutable counterpart is [`MailState`](super::MailState), whose home is the `message` row
/// and the `membership` junction. [`Message::keywords`] is therefore absent here by
/// construction, and [`Message::thread`] is present only when the **provider** assigned it — a
/// derived thread is the engine's, and it re-keys whenever new mail joins the conversation. A
/// payload carrying a copy of either could disagree with its home, and the disagreement would be
/// invisible until someone read the wrong one. There is no copy.
///
/// Borrowed and serialize-only: a sync page writes hundreds of these, and the JSON is the cost,
/// not another owned message beside it.
#[derive(Debug, Serialize)]
pub struct MailContent<'a> {
    id: &'a MessageId,
    /// Present only when the **provider** assigned the thread, because then it is the provider's
    /// word about the message and belongs with the rest of what it said.
    ///
    /// A locally-derived id is not: the engine computed it from the reference graph and rewrites
    /// it whenever new mail joins the conversation, so it lives in the `message` row alone. The
    /// distinction is load-bearing — the derivation pass reads this back to know which messages
    /// it must leave alone, and a stray `References` header must never merge two threads a
    /// provider kept apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    thread: Option<&'a ThreadRef>,
    blob_id: &'a Option<BlobId>,
    mailboxes: &'a Memberships<MailboxId>,
    size: &'a Option<u64>,
    received_at: &'a Option<UtcDateTime>,
    sent_at: &'a Option<UtcDateTime>,
    envelope: &'a Envelope,
    preview: &'a Option<String>,
    has_attachment: &'a bool,
    mime_structure: &'a Option<EmailBodyPart>,
    attachments: &'a Vec<Attachment>,
    reply_unique_text: &'a Option<String>,
    extended: &'a ExtendedProperties,
}

impl<'a> From<&'a Message> for MailContent<'a> {
    /// Destructured rather than field-by-field: a new field on [`Message`] stops this compiling
    /// until someone decides whether it belongs in the payload or in the row. A silent default
    /// is exactly the failure this type exists to prevent.
    fn from(message: &'a Message) -> Self {
        let Message {
            id,
            blob_id,
            thread,
            // Mutable state whose home is the message row and the membership junction.
            keywords: _,
            mailboxes,
            size,
            received_at,
            sent_at,
            envelope,
            preview,
            has_attachment,
            mime_structure,
            attachments,
            reply_unique_text,
            // Mutable state: revision tokens bump on a state-only change, and the modification
            // time is when that state moved. Both live in the message row.
            revisions: _,
            last_modified: _,
            extended,
        } = message;
        Self {
            id,
            thread: thread.as_ref().filter(|t| !t.is_derived()),
            blob_id,
            mailboxes,
            size,
            received_at,
            sent_at,
            envelope,
            preview,
            has_attachment,
            mime_structure,
            attachments,
            reply_unique_text,
            extended,
        }
    }
}

impl Message {
    /// Creates a message with the given id and mailbox membership, and empty
    /// defaults elsewhere.
    #[must_use]
    pub fn new(id: MessageId, mailboxes: Memberships<MailboxId>) -> Self {
        Self {
            id,
            blob_id: None,
            thread: None,
            mailboxes,
            keywords: BTreeSet::new(),
            size: None,
            received_at: None,
            sent_at: None,
            last_modified: None,
            envelope: Envelope::default(),
            preview: None,
            has_attachment: false,
            mime_structure: None,
            attachments: Vec::new(),
            reply_unique_text: None,
            revisions: RevisionTokens::none(),
            extended: ExtendedProperties::new(),
        }
    }

    /// The id of the thread this message belongs to, whatever its provenance.
    #[must_use]
    pub fn thread_id(&self) -> Option<&ThreadId> {
        self.thread.as_ref().map(|thread| &thread.id)
    }

    /// Returns `true` if the given keyword is set.
    #[must_use]
    pub fn has_keyword(&self, keyword: &Keyword) -> bool {
        self.keywords.contains(keyword)
    }

    /// Returns `true` if the given system keyword is set.
    #[must_use]
    pub fn has_system_keyword(&self, keyword: SystemKeyword) -> bool {
        self.keywords.contains(&Keyword::system(keyword))
    }

    /// Returns `true` if the message is a draft (`$draft`).
    #[must_use]
    pub fn is_draft(&self) -> bool {
        self.has_system_keyword(SystemKeyword::Draft)
    }

    /// Returns `true` if the message is unread.
    ///
    /// Per RFC 8621 §2, a message counts as unread when it has neither `$seen`
    /// nor `$draft` — a draft is never "unread".
    #[must_use]
    pub fn is_unread(&self) -> bool {
        !self.has_system_keyword(SystemKeyword::Seen) && !self.is_draft()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_stored_record_carries_no_keywords_and_no_derived_thread() {
        // The contract this type exists for. Asserted on the *rendered JSON keys* rather than on
        // the struct, because the payload is what a store writes and what a later reader decodes.
        let mut message = message("m1", "inbox");
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Seen));
        message.thread = Some(ThreadRef::derived(ThreadId::try_from("t1").unwrap()));

        let payload = serde_json::to_value(MailContent::from(&message)).unwrap();
        let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
        assert!(
            !keys.iter().any(|k| *k == "keywords"),
            "keywords live in the message row and the membership junction, got: {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| *k == "thread"),
            "a derived thread lives in the message row, got: {keys:?}"
        );
        // The content the provider did send is all still there.
        assert!(keys.iter().any(|k| *k == "envelope"));
        assert!(keys.iter().any(|k| *k == "mailboxes"));
    }

    #[test]
    fn the_stored_record_keeps_a_provider_assigned_thread() {
        // The provider said it, so it is part of what the provider said. The derivation pass
        // reads it back to know which mail it must not re-thread; dropping it would let a stray
        // `References` header merge two threads a provider kept apart.
        let mut message = message("m1", "inbox");
        message.thread = Some(ThreadRef::provider_assigned(
            ThreadId::try_from("T42").unwrap(),
        ));

        let payload = serde_json::to_value(MailContent::from(&message)).unwrap();
        let decoded: Message = serde_json::from_value(payload).unwrap();
        assert_eq!(decoded.thread_id().map(ThreadId::as_str), Some("T42"));
        assert!(!decoded.thread.unwrap().is_derived());
    }

    #[test]
    fn a_stored_payload_decodes_without_the_state_it_no_longer_carries() {
        // The mirror of the two tests above: `MailContent` omits these keys, so every payload
        // this build writes lacks them and `#[serde(default)]` is what lets one decode at all.
        // Without it, reading back a message the store had just written would fail.
        let decoded: Message = serde_json::from_value(serde_json::json!({
            "id": "m1",
            "mailboxes": ["inbox"],
            "envelope": Envelope::default(),
            "has_attachment": false,
            "attachments": [],
            "revisions": RevisionTokens::default(),
            "extended": ExtendedProperties::default(),
        }))
        .expect("a payload with neither field decodes");
        assert!(decoded.keywords.is_empty());
        assert!(decoded.thread.is_none());
    }

    use super::*;

    fn message(id: &str, mailbox: &str) -> Message {
        Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
        )
    }

    #[test]
    fn new_message_is_unread_until_seen() {
        let mut msg = message("m1", "inbox");
        assert!(msg.is_unread());
        assert!(!msg.is_draft());

        msg.keywords.insert(Keyword::system(SystemKeyword::Seen));
        assert!(!msg.is_unread());
    }

    #[test]
    fn a_draft_is_not_unread() {
        let mut msg = message("m1", "drafts");
        msg.keywords.insert(Keyword::system(SystemKeyword::Draft));
        assert!(msg.is_draft());
        assert!(!msg.is_unread());
    }

    #[test]
    fn user_keyword_lookup() {
        let mut msg = message("m1", "inbox");
        let project = Keyword::new("project-x").unwrap();
        assert!(!msg.has_keyword(&project));
        msg.keywords.insert(project.clone());
        assert!(msg.has_keyword(&project));
    }

    #[test]
    fn roundtrips_through_json() {
        let mut msg = message("m1", "inbox");
        msg.keywords.insert(Keyword::system(SystemKeyword::Flagged));
        msg.size = Some(2048);
        msg.received_at = Some("2021-01-01T12:00:00Z".parse().unwrap());
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), msg);
    }
}
