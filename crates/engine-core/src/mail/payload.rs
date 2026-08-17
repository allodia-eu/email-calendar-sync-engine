//! The two halves a stored message is kept in: the payload it is written as, and the payload
//! decoded back.
//!
//! Split from [`message`](Message) because this is the *storage* contract, not the
//! normalized object: what a payload may carry, what it may not, and the one seam that rejoins it
//! with the state the `message` row and the `membership` junction hold.

use serde::{Deserialize, Serialize};

use super::{EmailBodyPart, Envelope, Message, StoredState, ThreadRef};
use crate::{
    attachment::Attachment,
    extended::ExtendedProperties,
    ids::{BlobId, MessageId},
    time::UtcDateTime,
};

/// The mail payload the store persists: a message's **immutable half**.
///
/// A message's content never changes once the server holds it. Editing a draft is not a
/// counterexample — it mints a *new* provider object on every protocol we speak (a JMAP `Email`
/// is immutable, IMAP does APPEND + EXPUNGE and the UID changes). So this can be written once
/// and never reconciled.
///
/// Its mutable counterpart is [`MailState`](super::MailState), whose home is the `message` row
/// and the `membership` junction. [`Message::keywords`] and [`Message::mailboxes`] are therefore
/// absent here by construction, and [`Message::thread`] is present only when the **provider**
/// assigned it — a derived thread is the engine's, and it re-keys whenever new mail joins the
/// conversation. A payload carrying a copy of any of them could disagree with its home, and the
/// disagreement would be invisible until someone read the wrong one. There is no copy.
///
/// Filing is the axis that most looks like content and is not: JMAP and Gmail move a message
/// between mailboxes under a stable id, so a payload copy would go stale on any archive.
///
/// Borrowed and serialize-only, because a sync page writes hundreds of these and the JSON is the
/// cost. [`StoredContent`] is the owned counterpart it decodes back into.
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
            mailboxes: _,
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

/// A stored payload decoded back — the owned, read counterpart of [`MailContent`].
///
/// **This, not [`Message`], is what a stored payload deserializes into.** A `Message` needs its
/// mutable half too, and the only way to get one out of storage is [`Message::from_parts`]. That
/// is the guarantee: not that a reader remembers to overlay the state, but that a payload alone
/// cannot be mistaken for a whole message. Four live assertions once read a keyword off a
/// payload-decoded `Message` and passed by asserting a flag had not moved; this type is what
/// makes that unwritable.
///
/// Unknown fields are ignored, so a payload written before a field moved into the row still
/// decodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredContent {
    /// The provider object id.
    pub id: MessageId,
    /// The thread the **provider** assigned, if it assigned one. Absent for a message the engine
    /// threads itself, whose derived id lives in the message row.
    #[serde(default)]
    pub thread: Option<ThreadRef>,
    /// The blob holding the raw RFC 5322 bytes.
    pub blob_id: Option<BlobId>,
    /// The size of the raw message in octets, if known.
    pub size: Option<u64>,
    /// The delivery/internal date.
    pub received_at: Option<UtcDateTime>,
    /// The instant from the `Date` header.
    pub sent_at: Option<UtcDateTime>,
    /// The parsed addressing/threading headers.
    pub envelope: Envelope,
    /// A short snippet for list views.
    pub preview: Option<String>,
    /// Whether the message has a non-inline attachment.
    pub has_attachment: bool,
    /// The full normalized MIME tree, when synced.
    pub mime_structure: Option<EmailBodyPart>,
    /// The normalized attachments.
    pub attachments: Vec<Attachment>,
    /// The reply-unique body text, when available.
    pub reply_unique_text: Option<String>,
    /// Preserved provider-defined extended properties.
    pub extended: ExtendedProperties,
}

impl Message {
    /// Rebuilds a whole message from the two halves the store keeps apart.
    ///
    /// **The only way to turn stored mail back into a `Message`.** Written as a struct literal on
    /// purpose — a new field on `Message` stops this compiling until someone says which half it
    /// comes from, the same guard [`MailContent::from`] applies on the way out.
    ///
    /// A provider-assigned thread comes back with the payload and wins; only when the provider
    /// assigned none does the engine's derived id apply. Relabelling a provider's thread as
    /// derived would tell the derivation pass it may re-thread mail the provider threaded.
    #[must_use]
    pub fn from_parts(content: StoredContent, state: StoredState) -> Self {
        let StoredContent {
            id,
            thread,
            blob_id,
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
        } = content;
        let StoredState {
            mailboxes,
            keywords,
            thread: derived_thread,
            revisions,
            last_modified,
        } = state;
        Self {
            id,
            blob_id,
            thread: thread.or(derived_thread),
            mailboxes,
            keywords,
            size,
            received_at,
            sent_at,
            last_modified,
            envelope,
            preview,
            has_attachment,
            mime_structure,
            attachments,
            reply_unique_text,
            revisions,
            extended,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ids::{MailboxId, ThreadId},
        mail::{Keyword, SystemKeyword},
        membership::Memberships,
    };

    fn message(id: &str, mailbox: &str) -> Message {
        Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
        )
    }

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
        assert!(
            !keys.iter().any(|k| *k == "mailboxes"),
            "filing lives in the membership junction: JMAP and Gmail move a message under a \
             stable id, so a payload copy goes stale on any archive, got: {keys:?}"
        );
        // The content the provider did send is all still there.
        assert!(keys.iter().any(|k| *k == "envelope"));
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
        let decoded: StoredContent = serde_json::from_value(payload).unwrap();
        let thread = decoded.thread.expect("the provider's thread survives");
        assert_eq!(thread.id.as_str(), "T42");
        assert!(!thread.is_derived());
    }

    #[test]
    fn a_payload_carrying_no_state_at_all_still_decodes() {
        // What `MailContent` actually writes: none of the state keys. `StoredContent` is shaped
        // to that, so a stored payload decodes on its own — and is visibly not a whole message,
        // which is the point. Only `from_parts` can make one.
        let decoded: StoredContent = serde_json::from_value(serde_json::json!({
            "id": "m1",
            "envelope": Envelope::default(),
            "has_attachment": false,
            "attachments": [],
            "extended": ExtendedProperties::default(),
        }))
        .expect("a payload with no state decodes");
        assert_eq!(decoded.id.as_str(), "m1");
        assert!(decoded.thread.is_none());
    }

    #[test]
    fn a_key_the_payload_no_longer_writes_is_ignored_rather_than_fatal() {
        // Filing and keywords used to ride the payload, and a store written by a build that wrote
        // them still has those keys.
        //
        // **Nothing will ever remove them.** A migration is DDL — none has rewritten
        // `object.payload` and none can — and changing the payload's *shape* moves no schema
        // version, so opening such a store runs no migration at all. The keys stay until that
        // message is next re-synced, which nothing forces.
        //
        // So the decode has to tolerate them. `deny_unknown_fields` on `StoredContent` would read
        // as hardening and would instead fail every read of a store an earlier build wrote. That
        // is what this pins.
        let decoded: StoredContent = serde_json::from_value(serde_json::json!({
            "id": "m1",
            "mailboxes": ["inbox"],
            "keywords": ["$seen"],
            "envelope": Envelope::default(),
            "has_attachment": false,
            "attachments": [],
            "extended": ExtendedProperties::default(),
        }))
        .expect("a payload carrying keys the current shape omits still decodes");
        assert_eq!(decoded.id.as_str(), "m1");
    }

    #[test]
    fn a_message_survives_the_split_and_the_rejoin() {
        // The whole contract in one pass: split a message into the two halves the store keeps
        // apart, write and read the payload exactly as the store does, and rebuild. Anything
        // that stops being carried by *either* half stops surviving this.
        let mut msg = message("m1", "inbox");
        msg.keywords.insert(Keyword::system(SystemKeyword::Flagged));
        msg.thread = Some(ThreadRef::derived(ThreadId::try_from("t1").unwrap()));
        msg.size = Some(2048);
        msg.received_at = Some("2021-01-01T12:00:00Z".parse().unwrap());
        msg.envelope.subject = Some("a subject".to_owned());
        msg.last_modified = Some("2021-01-02T00:00:00Z".parse().unwrap());

        let payload = serde_json::to_string(&MailContent::from(&msg)).unwrap();
        let content: StoredContent = serde_json::from_str(&payload).unwrap();
        let rebuilt = Message::from_parts(
            content,
            StoredState {
                mailboxes: msg.mailboxes.clone(),
                keywords: msg.keywords.clone(),
                thread: msg.thread.clone(),
                revisions: msg.revisions.clone(),
                last_modified: msg.last_modified,
            },
        );
        assert_eq!(rebuilt, msg);
    }
}
