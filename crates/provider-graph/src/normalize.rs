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

use engine_core::ids::{MailboxId, MessageId, MessageIdHeader, ThreadId};
use engine_core::mail::{EmailAddress, Keyword, Mailbox, MailboxRole, Message, SystemKeyword};
use engine_core::membership::Memberships;
use engine_core::version::{ChangeKey, ETag, RevisionTokens};
use serde_json::Value;

use crate::error::GraphError;
use crate::json::{bool_field, datetime, opt_str, req_str, wrap_id};

/// The message properties the provider requests via `$select` — exactly the fields
/// [`message_from_json`] reads. Tier-1 metadata: the body/MIME are fetched on
/// demand, not here.
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
        message.thread_id = Some(wrap_id(ThreadId::try_from(thread), "conversation id")?);
    }
    // Graph models read/draft/flag as their own booleans, not a keyword set.
    if bool_field(value, "isRead") {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Seen));
    }
    if bool_field(value, "isDraft") {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Draft));
    }
    if flag_is_flagged(value) {
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Flagged));
    }
    message.has_attachment = bool_field(value, "hasAttachments");
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

/// `true` when `flag.flagStatus == "flagged"`.
fn flag_is_flagged(value: &Value) -> bool {
    value
        .get("flag")
        .and_then(|flag| flag.get("flagStatus"))
        .and_then(Value::as_str)
        == Some("flagged")
}

/// Truncates a body preview to the model's 256-character snippet bound.
fn snippet(text: &str) -> String {
    text.chars().take(256).collect()
}

/// The revision tokens Graph supplies: the `@odata.etag` and the `changeKey` (both
/// requested in `MESSAGE_SELECT`). Absent on a delta *partial* entry that did not
/// change them. JMAP-style accounts carry none.
fn revisions(value: &Value) -> RevisionTokens {
    RevisionTokens {
        etag: opt_str(value, "@odata.etag").map(ETag::new),
        change_key: opt_str(value, "changeKey").map(ChangeKey::new),
        ..RevisionTokens::none()
    }
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
mod tests {
    use super::*;

    const MAILFOLDERS: &str = include_str!("../tests/fixtures/mail/mailfolders.json");
    const SNAPSHOT: &str = include_str!("../tests/fixtures/mail/messages_delta_snapshot.json");
    const DETAIL: &str = include_str!("../tests/fixtures/mail/message_detail.json");

    /// The captured `msgfolderroot` id every top-level folder parents to.
    const ROOT: &str = "folder-root";

    fn folders() -> Vec<Mailbox> {
        let doc: Value = serde_json::from_str(MAILFOLDERS).unwrap();
        let root = MailboxId::try_from(ROOT).unwrap();
        doc["value"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| folder_from_json(f, Some(&root)).unwrap())
            .collect()
    }

    /// Builds the id → role map from the captured well-known-folder responses,
    /// exactly as the provider does from `GET /me/mailFolders/{alias}`.
    fn resolved_roles() -> Vec<(MailboxId, MailboxRole)> {
        let aliases = [
            (
                "inbox",
                include_str!("../tests/fixtures/wellknown/inbox.json"),
            ),
            (
                "archive",
                include_str!("../tests/fixtures/wellknown/archive.json"),
            ),
            (
                "drafts",
                include_str!("../tests/fixtures/wellknown/drafts.json"),
            ),
            (
                "sentitems",
                include_str!("../tests/fixtures/wellknown/sentitems.json"),
            ),
            (
                "deleteditems",
                include_str!("../tests/fixtures/wellknown/deleteditems.json"),
            ),
            (
                "junkemail",
                include_str!("../tests/fixtures/wellknown/junkemail.json"),
            ),
        ];
        WELL_KNOWN_ROLES
            .iter()
            .map(|(alias, role)| {
                let fixture = aliases.iter().find(|(a, _)| a == alias).unwrap().1;
                let id = well_known_folder_id(&serde_json::from_str(fixture).unwrap()).unwrap();
                (id, role.clone())
            })
            .collect()
    }

    fn messages() -> Vec<Message> {
        let doc: Value = serde_json::from_str(SNAPSHOT).unwrap();
        doc["value"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| message_from_json(m).unwrap())
            .collect()
    }

    fn by_subject(subject: &str) -> Message {
        messages()
            .into_iter()
            .find(|m| m.envelope.subject.as_deref() == Some(subject))
            .unwrap_or_else(|| panic!("no message with subject {subject}"))
    }

    #[test]
    fn folders_are_top_level_with_localized_names() {
        let all = folders();
        assert_eq!(all.len(), 8);
        // Every folder parents to msgfolderroot, so all are top-level (parent None).
        assert!(all.iter().all(|m| m.parent.is_none()));
        // Display names are localized (Dutch) — proving role mapping can't read them.
        assert!(all.iter().any(|m| m.name == "Postvak IN"));
        assert!(all.iter().any(|m| m.name == "Verzonden items"));
        // Roleless until the well-known resolution is applied.
        assert!(all.iter().all(|m| m.role.is_none()));
    }

    #[test]
    fn roles_resolve_by_id_not_by_localized_name() {
        let mut all = folders();
        apply_roles(&mut all, &resolved_roles());
        let role_of = |name: &str| all.iter().find(|m| m.name == name).unwrap().role.clone();
        // The folder named "Postvak IN" gets Inbox purely by id match.
        assert_eq!(role_of("Postvak IN"), Some(MailboxRole::Inbox));
        assert_eq!(role_of("Verzonden items"), Some(MailboxRole::Sent));
        assert_eq!(role_of("Verwijderde items"), Some(MailboxRole::Trash));
        assert_eq!(role_of("Ongewenste e-mail"), Some(MailboxRole::Junk));
        assert_eq!(role_of("Concepten"), Some(MailboxRole::Drafts));
        assert_eq!(role_of("Archiveren"), Some(MailboxRole::Archive));
        // Outbox and Conversation History have no standard role.
        assert_eq!(role_of("Postvak UIT"), None);
        assert_eq!(role_of("Gesprekgeschiedenis"), None);
    }

    #[test]
    fn child_folder_keeps_a_non_root_parent() {
        // A folder nested under another (not msgfolderroot) keeps its parent.
        let child = serde_json::json!({
            "id": "child", "displayName": "Sub", "parentFolderId": "folder-inbox"
        });
        let root = MailboxId::try_from(ROOT).unwrap();
        let mailbox = folder_from_json(&child, Some(&root)).unwrap();
        assert_eq!(mailbox.parent.as_ref().unwrap().as_str(), "folder-inbox");
    }

    #[test]
    fn message_normalizes_tier1_fields() {
        let msg = by_subject("Fixture: first message");
        // Single-folder membership from parentFolderId.
        assert_eq!(msg.mailboxes.len().get(), 1);
        assert!(
            msg.mailboxes
                .contains(&MailboxId::try_from("folder-inbox").unwrap())
        );
        // Self-addressed deterministic fixture: from/to is the scrubbed account.
        assert_eq!(msg.envelope.from[0].email, "testuser@example.test");
        assert_eq!(msg.envelope.to[0].email, "testuser@example.test");
        // internetMessageId is preserved bracket-stripped as a threading hint.
        let message_id = msg.envelope.message_id[0].as_str();
        assert!(
            !message_id.starts_with('<') && message_id.ends_with("@example.test"),
            "bracket-stripped Message-ID, got {message_id:?}"
        );
        // conversationId → thread provenance; etag → revision token.
        assert!(msg.thread_id.is_some());
        assert!(msg.revisions.etag.is_some());
        // Captured unread, not a draft, not flagged.
        assert!(msg.is_unread());
        assert!(!msg.is_draft());
        assert!(!msg.has_system_keyword(SystemKeyword::Flagged));
        assert!(!msg.has_attachment);
        assert!(msg.received_at.is_some() && msg.sent_at.is_some());
        assert!(msg.preview.is_some());
    }

    #[test]
    fn full_message_get_carries_change_key_and_last_modified() {
        let doc: Value = serde_json::from_str(DETAIL).unwrap();
        let msg = message_from_json(&doc).unwrap();
        // The full GET (unlike a delta entry) carries the changeKey + modified time.
        assert!(msg.revisions.change_key.is_some());
        assert!(msg.revisions.etag.is_some());
        assert!(msg.last_modified.is_some());
    }

    #[test]
    fn internet_message_id_brackets_are_stripped_and_empty_is_dropped() {
        let with = serde_json::json!({
            "id": "m", "parentFolderId": "folder-inbox",
            "internetMessageId": "  <abc@host>  "
        });
        let msg = message_from_json(&with).unwrap();
        assert_eq!(msg.envelope.message_id[0].as_str(), "abc@host");

        // An empty/bracket-only id is dropped, not an error.
        let without = serde_json::json!({
            "id": "m", "parentFolderId": "folder-inbox", "internetMessageId": "<>"
        });
        assert!(
            message_from_json(&without)
                .unwrap()
                .envelope
                .message_id
                .is_empty()
        );
    }

    #[test]
    fn flagged_and_draft_booleans_become_keywords() {
        let json = serde_json::json!({
            "id": "m", "parentFolderId": "folder-drafts",
            "isRead": true, "isDraft": true, "flag": { "flagStatus": "flagged" }
        });
        let msg = message_from_json(&json).unwrap();
        assert!(msg.has_system_keyword(SystemKeyword::Seen));
        assert!(msg.has_system_keyword(SystemKeyword::Draft));
        assert!(msg.has_system_keyword(SystemKeyword::Flagged));
    }

    #[test]
    fn malformed_messages_are_protocol_errors_not_panics() {
        // No id.
        assert!(message_from_json(&serde_json::json!({ "parentFolderId": "f" })).is_err());
        // No parentFolderId → no membership.
        assert!(message_from_json(&serde_json::json!({ "id": "m" })).is_err());
        // An empty parentFolderId is an invalid id.
        assert!(
            message_from_json(&serde_json::json!({ "id": "m", "parentFolderId": "" })).is_err()
        );
        // A malformed timestamp surfaces as a protocol error, never a panic.
        assert!(
            message_from_json(&serde_json::json!({
                "id": "m", "parentFolderId": "folder-inbox", "receivedDateTime": "not-a-date"
            }))
            .is_err()
        );
    }

    #[test]
    fn a_folder_without_a_parent_is_top_level() {
        // No parentFolderId at all → top-level (parent None), no `root` comparison.
        let mailbox =
            folder_from_json(&serde_json::json!({ "id": "f", "displayName": "F" }), None).unwrap();
        assert!(mailbox.parent.is_none());
    }
}
