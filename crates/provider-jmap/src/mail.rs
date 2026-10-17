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

use std::collections::BTreeSet;

use engine_core::{
    ids::{BlobId, MailboxId, MessageId, MessageIdHeader, ThreadId},
    mail::{
        EmailAddress, Keyword, MailState, MailStateChange, Mailbox, MailboxAccess, MailboxRole,
        Message, ThreadRef,
    },
    membership::Memberships,
};
use serde_json::Value;

use crate::{
    error::JmapError,
    json::{datetime, opt_str, req_str, sent_at, true_keys, wrap_id},
};

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
    // RFC 8621 §2.1 `myRights`; a server that omits it has told us nothing to withhold.
    if let Some(may) = value
        .pointer("/myRights/mayCreateChild")
        .and_then(Value::as_bool)
    {
        mailbox.accepts_children = may;
    }
    // RFC 8621 §2: `unreadEmails`, deliberately not `unreadThreads` — the engine's
    // count is of messages, the only form every transport reports. A `Mailbox/get`
    // with no `properties` returns the whole object, so nothing is asked for here.
    mailbox.unread_count = value
        .get("unreadEmails")
        .and_then(Value::as_u64)
        .map(|count| u32::try_from(count).unwrap_or(u32::MAX));
    mailbox.access = mailbox_rights(value.get("myRights"));
    // RFC 8621 §2: without `mayReadItems` no `Email/query` may filter on the mailbox, so to the
    // user it is a level of the hierarchy, as a shared account's parent folder often is.
    mailbox.selectable = value
        .pointer("/myRights/mayReadItems")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok(mailbox)
}

/// Reads a JMAP `MailboxRights` object (RFC 8621 §2) as [`MailboxAccess`].
///
/// A right missing *from* the object reads as withheld — the safe direction for a
/// permission. The object missing altogether is a different thing: a server that reports no
/// rights, whose folders are the user's own, and the honest reading there is
/// [`MailboxAccess::owner`], what every caller assumed before rights were read at all.
///
/// `mayShare` is not in RFC 8621, but servers implementing mailbox sharing report it (Stalwart
/// does), so it is read where present. `maySubmit` is not read: it says mail may be posted
/// *into* the mailbox, which no client action depends on (`MailboxAccess`).
fn mailbox_rights(rights: Option<&Value>) -> MailboxAccess {
    let Some(rights) = rights.filter(|rights| rights.is_object()) else {
        return MailboxAccess::owner();
    };
    let may = |name: &str| rights.get(name).and_then(Value::as_bool).unwrap_or(false);
    MailboxAccess {
        may_read_items: may("mayReadItems"),
        may_add_items: may("mayAddItems"),
        may_remove_items: may("mayRemoveItems"),
        may_set_seen: may("maySetSeen"),
        may_set_keywords: may("maySetKeywords"),
        may_create_child: may("mayCreateChild"),
        may_rename: may("mayRename"),
        may_delete: may("mayDelete"),
        may_share: may("mayShare"),
    }
}

/// The `Email` properties a **state-only** read needs.
///
/// RFC 8621 §4.1 makes `keywords` and `mailboxIds` the only mutable `Email` properties, so these
/// two plus the id are the whole of what an `Email/changes` `updated` id can be reporting.
pub(crate) const EMAIL_STATE_PROPERTIES: &[&str] = &["id", "keywords", "mailboxIds"];

/// Reads one `Email`'s complete mutable state, as returned for [`EMAIL_STATE_PROPERTIES`].
///
/// JMAP objects carry no per-message revision token and no modification time — the account-wide
/// `state` string is the cursor, not a per-object version — so those stay empty, which is what a
/// JMAP message's row already holds.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if `id` is missing, `mailboxIds` is empty (RFC 8621 §4.1.1
/// requires at least one), or a keyword is malformed.
pub(crate) fn state_from_json(value: &Value) -> Result<MailStateChange, JmapError> {
    let id_str = req_str(value, "id")?;
    let mailbox_ids = true_keys(value, "mailboxIds")
        .map(|key| wrap_id(MailboxId::try_from(key), "mailbox id"))
        .collect::<Result<Vec<_>, _>>()?;
    let mailboxes = Memberships::new(mailbox_ids)
        .map_err(|_| JmapError::protocol(format!("email {id_str:?} has empty mailboxIds")))?;

    let mut keywords = BTreeSet::new();
    for keyword in true_keys(value, "keywords") {
        keywords.insert(
            Keyword::new(keyword)
                .map_err(|e| JmapError::protocol(format!("bad keyword {keyword:?}: {e}")))?,
        );
    }

    let key = wrap_id(MessageId::try_from(id_str), "message id")?
        .key()
        .clone();
    Ok(MailStateChange::new(
        key,
        MailState::with_keywords(keywords).filed_in(mailboxes),
    ))
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
    message.thread = match opt_str(value, "threadId") {
        Some(thread) => Some(ThreadRef::provider_assigned(wrap_id(
            ThreadId::try_from(thread),
            "thread id",
        )?)),
        None => None,
    };
    for keyword in true_keys(value, "keywords") {
        let parsed = Keyword::new(keyword)
            .map_err(|e| JmapError::protocol(format!("bad keyword {keyword:?}: {e}")))?;
        message.keywords.insert(parsed);
    }
    message.size = value.get("size").and_then(Value::as_u64);
    message.received_at = datetime(value, "receivedAt")?;
    message.sent_at = sent_at(value);
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
#[path = "mail_tests.rs"]
mod tests;
