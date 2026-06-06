//! Mail identity newtypes.

use serde::{Deserialize, Serialize};

use super::{IdError, ProviderKey};

object_id! {
    /// Identifies a mailbox, folder, or label collection within an account.
    ///
    /// JMAP `Mailbox.id` (RFC 8621 §2), Gmail label id, Graph `mailFolder.id`,
    /// or an IMAP mailbox key. Distinct from the collection's normalized role
    /// (inbox/sent/…) and from its display name.
    MailboxId
}

object_id! {
    /// Identifies a stored mail object (a *provider* object, not a deduplicated
    /// RFC 5322 message).
    ///
    /// In JMAP this is `Email.id`, stable across mailbox moves (RFC 8621 §2). In
    /// IMAP, where identity is `(mailbox, UIDVALIDITY, UID)` and a copy in
    /// another folder is a distinct object, the adapter synthesizes a stable
    /// key. Never equal to the RFC 5322 `Message-ID` header — see
    /// [`MessageIdHeader`].
    MessageId
}

object_id! {
    /// Identifies a thread. Thread ids carry provenance: provider-assigned where
    /// available (JMAP `Thread.id`, Gmail `threadId`, Graph `conversationId`),
    /// locally derived otherwise. Late-arriving messages can merge local
    /// threads.
    ThreadId
}

object_id! {
    /// References the raw bytes of a message or body part (JMAP `blobId`, RFC
    /// 8620 §6). Immutable bytes; **not stable across writes** — a provider may
    /// mint a new blob id for identical bytes, so re-read it after a write.
    BlobId
}

object_id! {
    /// Identifies a MIME body part within a message (JMAP `EmailBodyPart.partId`,
    /// RFC 8621 §4.1.4). Unique within its message; `null` for `multipart/*`
    /// containers in JMAP, so this type is only used where a part id exists.
    PartId
}

content_id! {
    /// The RFC 5322 `Message-ID` header value, with surrounding angle brackets
    /// removed (the JMAP `MessageIds` form, RFC 8621 §4.1.2.5).
    ///
    /// This is a **threading and reconciliation hint, not hard identity**: a
    /// message may carry a duplicate `Message-ID`, or none at all, so it can
    /// never be the primary key. It is used to reconcile a generated outgoing
    /// message with its synced-back copy and to thread replies.
    ///
    /// The 998-octet cap is the RFC 5322 maximum line length, applied
    /// defensively because mail is hostile input.
    MessageIdHeader,
    max_octets = 998
}
