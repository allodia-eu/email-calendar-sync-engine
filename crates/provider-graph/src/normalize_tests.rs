//! Unit tests for Graph mail normalization.

use engine_core::mail::SystemKeyword;

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
fn folder_carries_the_unread_count_the_default_projection_returns() {
    let root = MailboxId::try_from(ROOT).unwrap();
    let counted = folder_from_json(
        &serde_json::json!({
            "id": "f", "displayName": "Inbox",
            "totalItemCount": 1200, "unreadItemCount": 545
        }),
        Some(&root),
    )
    .unwrap();
    assert_eq!(counted.unread_count, Some(545));

    // A payload without the property leaves it absent — not zero, which would
    // claim the folder had been counted and found empty.
    let uncounted = folder_from_json(
        &serde_json::json!({ "id": "f", "displayName": "Inbox" }),
        Some(&root),
    )
    .unwrap();
    assert_eq!(uncounted.unread_count, None);
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
    // The conversation id is the provider's, never re-grouped by local derivation.
    assert!(msg.thread.as_ref().is_some_and(|t| !t.is_derived()));
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
    assert!(message_from_json(&serde_json::json!({ "id": "m", "parentFolderId": "" })).is_err());
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

#[test]
fn a_size_is_estimated_from_the_attachments_and_only_when_there_are_any() {
    use serde_json::json;

    let with_attachments = |attachments: serde_json::Value| {
        json!({
            "id": "m1",
            "parentFolderId": "f1",
            "hasAttachments": true,
            "attachments": attachments,
        })
    };

    // 1.44 MB of attachments is 1.99 MB stored, measured — base64 plus body and headers.
    let m = message_from_json(&with_attachments(json!([{ "size": 1_444_148 }]))).expect("message");
    let estimate = m
        .size
        .expect("a message with attachments carries an estimate");
    assert!(
        (1_950_000..2_150_000).contains(&estimate),
        "estimate {estimate} should bracket the 1.99 MB actually stored",
    );

    // Several attachments add up.
    let split = message_from_json(&with_attachments(
        json!([{ "size": 700_000 }, { "size": 744_148 }]),
    ))
    .expect("message");
    assert_eq!(
        split.size, m.size,
        "the estimate is over the total, not per part"
    );

    // No attachments is no opinion, never "small" — such a message is fetched whatever the cap.
    let bare = message_from_json(&json!({
        "id": "m1", "parentFolderId": "f1", "hasAttachments": false,
    }))
    .expect("message");
    assert_eq!(bare.size, None);
    let empty = message_from_json(&with_attachments(json!([]))).expect("message");
    assert_eq!(empty.size, None);
}
