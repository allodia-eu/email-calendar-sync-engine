//! Normalizing Microsoft Graph `mailFolder` and `message` JSON into the engine's
//! domain model.
//!
//! Pure `serde_json::Value` → [`Mailbox`]/[`Message`] conversion, unit-tested
//! offline against captured fixtures. It maps the three independent axes faithfully
//! (`modeling.md`): the Graph **immutable id** is identity; `parentFolderId` is the
//! single-folder membership (Graph mail is one-folder, like an IMAP copy, not the
//! multi-membership JMAP/Gmail shape); read/draft/flag booleans become the keyword
//! state axis. `internetMessageId` is preserved (bracket-stripped) as a threading
//! hint, never identity.
//!
//! Two Graph realities (captured — see `tests/fixtures/README.md`): a personal
//! `mailFolder` carries **no** `wellKnownName` and a **localized** `displayName`,
//! so [`MailboxRole`] is resolved by matching folder ids against the well-known
//! aliases ([`apply_roles`]), never by name; and an incremental `delta` returns
//! **partial** message objects, so [`message_from_json`] is only ever fed a *full*
//! object (a snapshot entry or a re-fetched changed message), never a delta partial.

use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader, ThreadId},
    mail::{EmailAddress, Mailbox, MailboxRole, Message, ThreadRef},
    membership::Memberships,
};
use serde_json::Value;

use crate::{
    error::GraphError,
    json::{bool_field, datetime, opt_str, req_str, wrap_id},
    normalize_state::{keywords_from_json, revisions},
};

/// The message properties the provider requests via `$select` — exactly the fields
/// [`message_from_json`] reads. Tier-1 metadata: the body/MIME are fetched on
/// demand, not here.
/// The `$expand` that carries attachment sizes alongside the metadata, so a size estimate costs
/// no extra request. Accepted on the **delta** endpoint, which is what makes it usable at all.
pub(crate) const MESSAGE_EXPAND: &str = "attachments($select=size)";

/// What the body and headers add on top of the attachments, in octets.
const BODY_ALLOWANCE: u64 = 128 * 1024;

/// Estimates the octets a warm would store, from the attachment sizes the metadata carries.
///
/// Graph exposes no message size on any endpoint the sync uses. `PR_MESSAGE_SIZE` exists as an
/// extended property, but the delta endpoint **rejects** expanding it (`400`, "Parsing OData
/// Select and Expand failed"), and it measures the MAPI store rather than MIME anyway.
///
/// Attachment sizes are expandable on delta, and compose the way MIME does: base64 costs the
/// textbook 4/3, and the body and headers add roughly a constant. Measured over 5,000 messages
/// of a multi-year mailbox, 67 of them fetched in full, this **never runs more than 2% under**
/// the bytes actually stored, and is near-exact exactly where a cap decides:
///
/// | on disk | n | estimate / actual |
/// |---|---|---|
/// | ≥ 5 MB | 6 | 0.98 – 1.00 |
/// | ≥ 2 MB | 12 | 0.98 – 2.04 |
/// | everything | 67 | median 1.39, up to 5.23 |
///
/// The big ones are dominated by ordinary attachments, where `size` *is* the content length —
/// across 36 of them the ratio to the base64 in the MIME was 1.326–1.331. The loose tail is all
/// small messages, where over-shooting cannot reach any cap a user can pick.
///
/// **An inline attachment does not need its own factor, and giving it one is worse.** Graph
/// reports inline parts at a size that bears no fixed relation to what the MIME carries —
/// measured message by message, the implied ratio runs from 0.64 to 1.26 — so any constant
/// tuned for them under-estimates the rest. Scaling inline sizes by 3/4 (fitted on three
/// attachments, which looked convincing) turned a worst case of 1.02× under into **1.56× under**
/// on 8 of 67 messages. Treating every attachment alike absorbs the variance into the safe
/// direction.
///
/// Reads the attachments collection, not `hasAttachments`, which is **not** the same question:
/// Graph reports that flag `false` for a message whose only attachments are inline — the
/// embedded images that carry no paperclip and plenty of bytes. 114 of those 5,000 messages
/// carry inline parts, and a cap keyed on the flag would have missed every one of them.
///
/// `None` for a message with nothing attached — not the allowance. Such a message is nowhere
/// near a cap, and "no opinion" is what leaves it always fetched.
fn estimated_size(value: &Value) -> Option<u64> {
    let total: u64 = value
        .get("attachments")?
        .as_array()?
        .iter()
        .filter_map(|attachment| attachment.get("size").and_then(Value::as_u64))
        .sum();
    (total > 0).then(|| total.saturating_mul(4) / 3 + BODY_ALLOWANCE)
}

pub(crate) const MESSAGE_SELECT: &[&str] = &[
    "id",
    "internetMessageId",
    "conversationId",
    "parentFolderId",
    "subject",
    "from",
    "sender",
    "toRecipients",
    "ccRecipients",
    "bccRecipients",
    "receivedDateTime",
    "sentDateTime",
    "lastModifiedDateTime",
    "isRead",
    "isDraft",
    "hasAttachments",
    "flag",
    "bodyPreview",
    "changeKey",
];

/// The well-known mail-folder aliases that map to a normalized [`MailboxRole`].
///
/// The provider `GET`s each alias to learn its folder id, then matches by id
/// ([`apply_roles`]) — display names are localized, so they cannot be parsed.
/// `outbox` (a transient send queue) and `msgfolderroot` have no standard role.
pub(crate) const WELL_KNOWN_ROLES: &[(&str, MailboxRole)] = &[
    ("inbox", MailboxRole::Inbox),
    ("archive", MailboxRole::Archive),
    ("drafts", MailboxRole::Drafts),
    ("sentitems", MailboxRole::Sent),
    ("deleteditems", MailboxRole::Trash),
    ("junkemail", MailboxRole::Junk),
];

/// Normalizes one Graph `mailFolder` into a **roleless** [`Mailbox`].
///
/// Role is assigned afterwards from the well-known-alias resolution
/// ([`apply_roles`]). A `parentFolderId` equal to `root` (the `msgfolderroot`) marks
/// a top-level folder, whose parent is `None`.
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] if the folder lacks a usable `id`.
pub(crate) fn folder_from_json(
    value: &Value,
    root: Option<&MailboxId>,
) -> Result<Mailbox, GraphError> {
    let id = wrap_id(MailboxId::try_from(req_str(value, "id")?), "mail folder id")?;
    let name = opt_str(value, "displayName").unwrap_or_default().to_owned();
    let mut mailbox = Mailbox::new(id, name);
    mailbox.parent = match opt_str(value, "parentFolderId") {
        Some(parent) => {
            let parent = wrap_id(MailboxId::try_from(parent), "parent folder id")?;
            (Some(&parent) != root).then_some(parent)
        }
        None => None,
    };
    // `unreadItemCount` rides along on the default `mailFolder` projection — the
    // list and the delta both carry it, so it costs no extra request. A payload
    // that omits it leaves the count absent rather than zeroing it.
    mailbox.unread_count = value
        .get("unreadItemCount")
        .and_then(Value::as_u64)
        .map(|count| u32::try_from(count).unwrap_or(u32::MAX));
    Ok(mailbox)
}

/// Reads the folder id from a single well-known-folder response (e.g. `GET
/// /me/mailFolders/inbox`), used to build the id → role map.
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] if the response lacks a usable `id`.
pub(crate) fn well_known_folder_id(value: &Value) -> Result<MailboxId, GraphError> {
    wrap_id(MailboxId::try_from(req_str(value, "id")?), "mail folder id")
}

/// Assigns roles to `mailboxes` by matching their ids against the resolved
/// well-known-folder ids — never by display name (which is localized).
pub(crate) fn apply_roles(mailboxes: &mut [Mailbox], resolved: &[(MailboxId, MailboxRole)]) {
    for mailbox in mailboxes {
        if let Some((_, role)) = resolved.iter().find(|(id, _)| *id == mailbox.id) {
            mailbox.role = Some(role.clone());
        }
    }
}

/// Normalizes one **full** Graph `message` into a [`Message`].
///
/// Used for snapshot entries and re-fetched changed messages — never the *partial*
/// objects an incremental `delta` returns (the provider re-fetches those first).
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] if `id` or `parentFolderId` is missing (Graph
/// mail always carries its single-folder membership) or a value is malformed.
pub(crate) fn message_from_json(value: &Value) -> Result<Message, GraphError> {
    let id = wrap_id(MessageId::try_from(req_str(value, "id")?), "message id")?;
    let folder = wrap_id(
        MailboxId::try_from(req_str(value, "parentFolderId")?),
        "parent folder id",
    )?;
    let mut message = Message::new(id, Memberships::of_one(folder));

    if let Some(thread) = opt_str(value, "conversationId") {
        message.thread = Some(ThreadRef::provider_assigned(wrap_id(
            ThreadId::try_from(thread),
            "conversation id",
        )?));
    }
    message.keywords = keywords_from_json(value);
    message.has_attachment = bool_field(value, "hasAttachments");
    message.size = estimated_size(value);
    message.received_at = datetime(value, "receivedDateTime")?;
    message.sent_at = datetime(value, "sentDateTime")?;
    message.last_modified = datetime(value, "lastModifiedDateTime")?;
    message.preview = opt_str(value, "bodyPreview").map(snippet);
    message.revisions = revisions(value);

    let envelope = &mut message.envelope;
    envelope.subject = opt_str(value, "subject").map(str::to_owned);
    envelope.from = single_address(value, "from");
    envelope.sender = single_address(value, "sender");
    envelope.to = recipients(value, "toRecipients");
    envelope.cc = recipients(value, "ccRecipients");
    envelope.bcc = recipients(value, "bccRecipients");
    if let Some(header) = message_id_header(value)? {
        envelope.message_id = vec![header];
    }
    Ok(message)
}

/// Truncates a body preview to the model's 256-character snippet bound.
fn snippet(text: &str) -> String {
    text.chars().take(256).collect()
}

/// One `{ emailAddress: { name, address } }` object as a 0-or-1 address list.
fn single_address(value: &Value, key: &str) -> Vec<EmailAddress> {
    value.get(key).and_then(email_address).into_iter().collect()
}

/// An array of `{ emailAddress: { name, address } }` recipients.
fn recipients(value: &Value, key: &str) -> Vec<EmailAddress> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(email_address)
        .collect()
}

/// Projects a Graph `recipient` (`{ emailAddress: { name, address } }`) to an
/// [`EmailAddress`]; `None` when the address is absent.
fn email_address(recipient: &Value) -> Option<EmailAddress> {
    let inner = recipient.get("emailAddress")?;
    let email = inner.get("address").and_then(Value::as_str)?;
    let name = inner.get("name").and_then(Value::as_str).map(str::to_owned);
    Some(EmailAddress {
        name,
        email: email.to_owned(),
    })
}

/// The bracket-stripped `internetMessageId`, or `None` when absent/empty.
fn message_id_header(value: &Value) -> Result<Option<MessageIdHeader>, GraphError> {
    let Some(raw) = opt_str(value, "internetMessageId") else {
        return Ok(None);
    };
    let trimmed = raw.trim().trim_start_matches('<').trim_end_matches('>');
    if trimmed.is_empty() {
        return Ok(None);
    }
    MessageIdHeader::new(trimmed)
        .map(Some)
        .map_err(|e| GraphError::protocol(format!("bad internetMessageId {raw:?}: {e}")))
}

#[cfg(test)]
#[path = "normalize_tests.rs"]
mod tests;
