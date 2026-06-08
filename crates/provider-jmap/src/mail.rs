//! Normalizing JMAP `Mailbox` and `Email` JSON into the engine's domain model.
//!
//! Pure `serde_json::Value` → [`Mailbox`]/[`Message`] conversion (RFC 8621
//! §1.6/§2 mailboxes, §4 emails), so it is fully unit-tested offline against
//! captured Stalwart transcripts. It maps the three independent axes faithfully
//! (`modeling.md`): the JMAP **object id** is identity; `mailboxIds` is the
//! non-empty membership set (so the COPY that JMAP presents as one object with two
//! memberships stays one [`Message`], while the duplicate-`Message-ID` pair stays
//! two distinct objects); `keywords` is the state axis. The `Message-ID` header is
//! preserved in the envelope as a hint, never used as identity.
//!
//! Tier-1 metadata only: the raw RFC 5322 source is referenced by `blobId` (for
//! on-demand fetch) but not materialized here — durable raw-MIME blob storage is a
//! later store sub-step (`docs/agent-guidance/jmap.md`).

use engine_core::ids::{BlobId, MailboxId, MessageId, MessageIdHeader, ThreadId};
use engine_core::mail::{EmailAddress, Keyword, Mailbox, MailboxRole, Message};
use engine_core::membership::Memberships;
use serde_json::Value;

use crate::error::JmapError;
use crate::json::{datetime, opt_str, req_str, true_keys, wrap_id};

/// The `Email` properties fetched in `Email/get` — exactly the fields
/// [`message_from_json`] reads (RFC 8621 §4.1). Tier-1 metadata: the body parts
/// and full MIME are not requested here.
pub(crate) const EMAIL_PROPERTIES: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "mailboxIds",
    "keywords",
    "size",
    "receivedAt",
    "sentAt",
    "subject",
    "from",
    "sender",
    "replyTo",
    "to",
    "cc",
    "bcc",
    "messageId",
    "inReplyTo",
    "references",
    "hasAttachment",
    "preview",
];

/// Normalizes one JMAP `Mailbox` object into a [`Mailbox`].
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the object lacks a usable `id`.
pub(crate) fn mailbox_from_json(value: &Value) -> Result<Mailbox, JmapError> {
    let id = wrap_id(MailboxId::try_from(req_str(value, "id")?), "mailbox id")?;
    let name = opt_str(value, "name").unwrap_or_default().to_owned();
    let mut mailbox = Mailbox::new(id, name);
    mailbox.parent = match opt_str(value, "parentId") {
        Some(parent) => Some(wrap_id(MailboxId::try_from(parent), "parent mailbox id")?),
        None => None,
    };
    // A null/absent role is a roleless collection (Stalwart's Archive/Projects);
    // a present role string maps through the IANA registry.
    mailbox.role = opt_str(value, "role").map(MailboxRole::from_jmap_role);
    if let Some(order) = value.get("sortOrder").and_then(Value::as_u64) {
        mailbox.sort_order = u32::try_from(order).unwrap_or(u32::MAX);
    }
    if let Some(subscribed) = value.get("isSubscribed").and_then(Value::as_bool) {
        mailbox.subscribed = subscribed;
    }
    Ok(mailbox)
}

/// Normalizes one JMAP `Email` object into a [`Message`].
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if `id` is missing, `mailboxIds` is empty
/// (RFC 8621 §4.1.1 requires at least one), or a keyword/Message-ID is malformed.
pub(crate) fn message_from_json(value: &Value) -> Result<Message, JmapError> {
    let id_str = req_str(value, "id")?;

    let mailbox_ids = true_keys(value, "mailboxIds")
        .map(|key| wrap_id(MailboxId::try_from(key), "mailbox id"))
        .collect::<Result<Vec<_>, _>>()?;
    let mailboxes = Memberships::new(mailbox_ids)
        .map_err(|_| JmapError::protocol(format!("email {id_str:?} has empty mailboxIds")))?;

    let id = wrap_id(MessageId::try_from(id_str), "message id")?;
    let mut message = Message::new(id, mailboxes);

    message.blob_id = match opt_str(value, "blobId") {
        Some(blob) => Some(wrap_id(BlobId::try_from(blob), "blob id")?),
        None => None,
    };
    message.thread_id = match opt_str(value, "threadId") {
        Some(thread) => Some(wrap_id(ThreadId::try_from(thread), "thread id")?),
        None => None,
    };
    for keyword in true_keys(value, "keywords") {
        let parsed = Keyword::new(keyword)
            .map_err(|e| JmapError::protocol(format!("bad keyword {keyword:?}: {e}")))?;
        message.keywords.insert(parsed);
    }
    message.size = value.get("size").and_then(Value::as_u64);
    message.received_at = datetime(value, "receivedAt")?;
    message.sent_at = datetime(value, "sentAt")?;
    message.has_attachment = value
        .get("hasAttachment")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    message.preview = opt_str(value, "preview").map(str::to_owned);

    let envelope = &mut message.envelope;
    envelope.subject = opt_str(value, "subject").map(str::to_owned);
    envelope.from = addresses(value, "from");
    envelope.sender = addresses(value, "sender");
    envelope.reply_to = addresses(value, "replyTo");
    envelope.to = addresses(value, "to");
    envelope.cc = addresses(value, "cc");
    envelope.bcc = addresses(value, "bcc");
    envelope.message_id = message_ids(value, "messageId")?;
    envelope.in_reply_to = message_ids(value, "inReplyTo")?;
    envelope.references = message_ids(value, "references")?;

    Ok(message)
}

/// A list of `EmailAddress` objects (`[{ name, email }]`), or empty for `null`.
fn addresses(value: &Value, key: &str) -> Vec<EmailAddress> {
    let Some(list) = value.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|entry| {
            let email = entry.get("email").and_then(Value::as_str)?;
            let name = entry.get("name").and_then(Value::as_str).map(str::to_owned);
            Some(EmailAddress {
                name,
                email: email.to_owned(),
            })
        })
        .collect()
}

/// A list of bracket-less `Message-ID` header values, or empty for `null`.
fn message_ids(value: &Value, key: &str) -> Result<Vec<MessageIdHeader>, JmapError> {
    let Some(list) = value.get(key).and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    list.iter()
        .filter_map(Value::as_str)
        .map(|raw| {
            MessageIdHeader::new(raw)
                .map_err(|e| JmapError::protocol(format!("bad Message-ID {raw:?}: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::mail::SystemKeyword;

    const MAILBOX_GET: &str = include_str!("../tests/fixtures/mailbox_get.json");
    const EMAIL_GET: &str = include_str!("../tests/fixtures/email_get.json");

    fn list(fixture: &str) -> Vec<Value> {
        let doc: Value = serde_json::from_str(fixture).unwrap();
        doc["list"].as_array().unwrap().clone()
    }

    fn mailboxes() -> Vec<Mailbox> {
        list(MAILBOX_GET)
            .iter()
            .map(|m| mailbox_from_json(m).unwrap())
            .collect()
    }

    fn messages() -> Vec<Message> {
        list(EMAIL_GET)
            .iter()
            .map(|e| message_from_json(e).unwrap())
            .collect()
    }

    /// Resolves a mailbox id from the captured fixture by role, never by its
    /// opaque server-assigned value (determinism rule).
    fn mailbox_by_role(role: &MailboxRole) -> MailboxId {
        mailboxes()
            .into_iter()
            .find(|m| m.role.as_ref() == Some(role))
            .unwrap_or_else(|| panic!("no mailbox with role {role:?}"))
            .id
    }

    fn mailbox_by_name(name: &str) -> MailboxId {
        mailboxes()
            .into_iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("no mailbox named {name}"))
            .id
    }

    fn by_subject(subject: &str) -> Message {
        messages()
            .into_iter()
            .find(|m| m.envelope.subject.as_deref() == Some(subject))
            .unwrap_or_else(|| panic!("no message with subject {subject}"))
    }

    #[test]
    fn mailboxes_map_roles_and_names() {
        let all = mailboxes();
        assert_eq!(all.len(), 7);
        // Special-use folders carry a normalized role; Stalwart's custom Archive
        // and Projects are roleless (mapped by name).
        assert!(!mailbox_by_role(&MailboxRole::Inbox).as_str().is_empty());
        assert!(all.iter().any(|m| m.name == "Archive" && m.role.is_none()));
        assert!(all.iter().any(|m| m.name == "Projects" && m.role.is_none()));
        assert!(all.iter().any(|m| m.role == Some(MailboxRole::Sent)));
        assert!(all.iter().any(|m| m.role == Some(MailboxRole::Trash)));
    }

    #[test]
    fn all_seed_emails_normalize() {
        assert_eq!(messages().len(), 9);
    }

    #[test]
    fn copy_in_archive_is_one_object_with_two_memberships() {
        // The IMAP COPY surfaces in JMAP as ONE Email object carrying both the
        // inbox and Archive memberships — the multi-membership view.
        let baseline = by_subject("Harness baseline message");
        assert_eq!(baseline.mailboxes.len().get(), 2);
        assert!(
            baseline
                .mailboxes
                .contains(&mailbox_by_role(&MailboxRole::Inbox))
        );
        assert!(baseline.mailboxes.contains(&mailbox_by_name("Archive")));
    }

    #[test]
    fn duplicate_message_id_stays_two_distinct_objects() {
        let copy_a = by_subject("Duplicate Message-ID (copy A)");
        let copy_b = by_subject("Duplicate Message-ID (copy B)");
        // Same Message-ID hint...
        assert_eq!(copy_a.envelope.message_id, copy_b.envelope.message_id);
        assert_eq!(
            copy_a.envelope.message_id[0].as_str(),
            "shared-dup-msgid@example.com"
        );
        // ...but distinct provider identity (never coalesced by Message-ID).
        assert_ne!(copy_a.id, copy_b.id);
    }

    #[test]
    fn missing_message_id_yields_empty_header_list() {
        let no_msgid = by_subject("Message with no Message-ID");
        assert!(no_msgid.envelope.message_id.is_empty());
        // Still a distinct, fully-normalized object.
        assert_eq!(no_msgid.mailboxes.len().get(), 1);
    }

    #[test]
    fn flagged_message_carries_system_and_user_keywords() {
        let flagged = by_subject("Message with flags and a custom keyword");
        assert!(flagged.has_system_keyword(SystemKeyword::Flagged));
        assert!(flagged.has_system_keyword(SystemKeyword::Seen));
        assert!(flagged.has_keyword(&Keyword::new("harness").unwrap()));
    }

    #[test]
    fn moved_message_has_single_projects_membership() {
        let moved = by_subject("Filed under Projects");
        assert_eq!(moved.mailboxes.len().get(), 1);
        assert!(moved.mailboxes.contains(&mailbox_by_name("Projects")));
        assert!(
            !moved
                .mailboxes
                .contains(&mailbox_by_role(&MailboxRole::Inbox))
        );
    }

    #[test]
    fn envelope_addresses_and_dates_normalize() {
        let baseline = by_subject("Harness baseline message");
        assert_eq!(baseline.envelope.from[0].email, "alice@test.local");
        assert_eq!(
            baseline.envelope.from[0].name.as_deref(),
            Some("Alice Tester")
        );
        assert_eq!(baseline.envelope.to[0].email, "bob@test.local");
        // cc was null → empty, not an error.
        assert!(baseline.envelope.cc.is_empty());
        // sentAt is the deterministic Date header; receivedAt is the (non-
        // deterministic) delivery time — both parse to instants.
        assert!(baseline.sent_at.is_some());
        assert!(baseline.received_at.is_some());
        assert!(baseline.blob_id.is_some());
    }

    #[test]
    fn mailbox_with_parent_and_metadata() {
        let mailbox = mailbox_from_json(&serde_json::json!({
            "id": "child", "name": "Clients", "parentId": "work", "role": "archive",
            "sortOrder": 5, "isSubscribed": false
        }))
        .unwrap();
        assert_eq!(mailbox.parent.as_ref().unwrap().as_str(), "work");
        assert_eq!(mailbox.role, Some(MailboxRole::Archive));
        assert_eq!(mailbox.sort_order, 5);
        assert!(!mailbox.subscribed);
    }

    #[test]
    fn malformed_email_is_a_protocol_error_not_a_panic() {
        // No id.
        assert!(message_from_json(&serde_json::json!({ "mailboxIds": { "a": true } })).is_err());
        // Empty mailboxIds violates RFC 8621 §4.1.1.
        assert!(message_from_json(&serde_json::json!({ "id": "x", "mailboxIds": {} })).is_err());
    }
}
