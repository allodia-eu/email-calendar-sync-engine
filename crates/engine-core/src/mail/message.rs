//! The normalized mail object.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{EmailBodyPart, Envelope, Keyword, SystemKeyword};
use crate::attachment::Attachment;
use crate::extended::ExtendedProperties;
use crate::ids::{BlobId, MailboxId, MessageId, ThreadId};
use crate::membership::Memberships;
use crate::time::UtcDateTime;
use crate::version::RevisionTokens;

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
    /// The thread this message belongs to, if threading is resolved.
    pub thread_id: Option<ThreadId>,
    /// The mailboxes/labels this message belongs to (always at least one).
    pub mailboxes: Memberships<MailboxId>,
    /// The keywords applied to this message.
    pub keywords: BTreeSet<Keyword>,
    /// The size of the raw message in octets, if known.
    pub size: Option<u64>,
    /// The delivery/internal date (IMAP internal date, JMAP `receivedAt`).
    pub received_at: Option<UtcDateTime>,
    /// The instant from the `Date` header (JMAP `sentAt`), normalized to UTC.
    pub sent_at: Option<UtcDateTime>,
    /// When the object was last modified at the provider.
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
    pub revisions: RevisionTokens,
    /// Preserved provider-defined extended properties.
    pub extended: ExtendedProperties,
}

impl Message {
    /// Creates a message with the given id and mailbox membership, and empty
    /// defaults elsewhere.
    #[must_use]
    pub fn new(id: MessageId, mailboxes: Memberships<MailboxId>) -> Self {
        Self {
            id,
            blob_id: None,
            thread_id: None,
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
