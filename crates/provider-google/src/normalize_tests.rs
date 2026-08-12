//! Offline normalization tests for Gmail labels and messages, driven by the captured
//! (scrubbed) fixtures under `tests/fixtures/mail/`.

use engine_core::mail::MailboxRole;
use serde_json::Value;

use super::*;

const LABELS: &str = include_str!("../tests/fixtures/mail/labels.json");
const META: &str = include_str!("../tests/fixtures/mail/message_metadata.json");
const META_LABELED: &str = include_str!("../tests/fixtures/mail/message_metadata_labeled.json");
const FULL: &str = include_str!("../tests/fixtures/mail/message_full.json");

fn labels() -> Vec<Mailbox> {
    let doc: Value = serde_json::from_str(LABELS).unwrap();
    doc["labels"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| label_from_json(l).unwrap())
        .collect()
}

fn message(fixture: &str) -> Message {
    message_from_json(&serde_json::from_str(fixture).unwrap()).unwrap()
}

#[test]
fn labels_map_system_ids_to_roles_and_keep_custom_labels_roleless() {
    let all = labels();
    let role = |id: &str| {
        all.iter()
            .find(|m| m.id.as_str() == id)
            .unwrap_or_else(|| panic!("no label {id}"))
            .role
            .clone()
    };
    assert_eq!(role("INBOX"), Some(MailboxRole::Inbox));
    assert_eq!(role("SENT"), Some(MailboxRole::Sent));
    assert_eq!(role("DRAFT"), Some(MailboxRole::Drafts));
    assert_eq!(role("TRASH"), Some(MailboxRole::Trash));
    assert_eq!(role("SPAM"), Some(MailboxRole::Junk));
    assert_eq!(role("IMPORTANT"), Some(MailboxRole::Important));
    // Custom label + category labels are roleless mailboxes.
    let custom = all.iter().find(|m| m.name == "Fixture Label").unwrap();
    assert_eq!(custom.role, None);
    assert!(
        all.iter()
            .any(|m| m.id.as_str() == "CATEGORY_UPDATES" && m.role.is_none())
    );
}

#[test]
fn keyword_only_labels_are_not_mailboxes() {
    // UNREAD/STARRED are keyword state, so the real labels.list entries for them do not
    // become mailboxes.
    let all = labels();
    assert!(!all.iter().any(|m| m.id.as_str() == "UNREAD"));
    assert!(!all.iter().any(|m| m.id.as_str() == "STARRED"));
    // The synthetic All Mail home carries the All role.
    let all_mail = all_mail_mailbox();
    assert_eq!(all_mail.id.as_str(), "ALL_MAIL");
    assert_eq!(all_mail.role, Some(MailboxRole::All));
}

#[test]
fn message_normalizes_tier1_fields() {
    let msg = message(META);
    // Multi-membership from labelIds, minus the keyword-only labels.
    assert!(
        msg.mailboxes
            .contains(&MailboxId::try_from("INBOX").unwrap())
    );
    assert!(
        msg.mailboxes
            .contains(&MailboxId::try_from("SENT").unwrap())
    );
    // UNREAD is a keyword, never a membership.
    assert!(
        !msg.mailboxes
            .contains(&MailboxId::try_from("UNREAD").unwrap())
    );
    // The self-sent fixture: from/to are the scrubbed account, with the display name.
    assert_eq!(msg.envelope.from[0].email, "testuser@example.test");
    assert_eq!(msg.envelope.from[0].name.as_deref(), Some("Test User"));
    assert_eq!(msg.envelope.to[0].email, "testuser@example.test");
    assert_eq!(
        msg.envelope.subject.as_deref(),
        Some("Fixture: first message")
    );
    // Message-Id is bracket-stripped, preserved as a threading hint.
    let mid = msg.envelope.message_id[0].as_str();
    assert!(!mid.starts_with('<') && mid == "message-1@mail.gmail.test");
    // threadId → provider-assigned thread, never derived.
    assert!(msg.thread.as_ref().is_some_and(|t| !t.is_derived()));
    // No per-message revision token (Gmail id is immutable, historyId is the cursor).
    assert!(msg.revisions.etag.is_none());
    // Captured unread (has UNREAD label) and not starred/draft.
    assert!(msg.is_unread());
    assert!(!msg.has_system_keyword(SystemKeyword::Flagged));
    assert!(!msg.is_draft());
    // internalDate → received_at; Date header → sent_at; snippet → preview.
    assert!(msg.received_at.is_some());
    assert!(msg.sent_at.is_some());
    assert_eq!(
        msg.preview.as_deref(),
        Some("The first fixture message body.")
    );
}

#[test]
fn a_labeled_message_carries_multi_membership_and_keywords() {
    let msg = message(META_LABELED);
    // Membership across INBOX, SENT, and the custom label at once.
    for id in ["INBOX", "SENT", "Label_1", "IMPORTANT"] {
        assert!(
            msg.mailboxes.contains(&MailboxId::try_from(id).unwrap()),
            "expected membership {id}"
        );
    }
    // STARRED → flagged keyword (not a membership); UNREAD removed → seen (read).
    assert!(msg.has_system_keyword(SystemKeyword::Flagged));
    assert!(
        !msg.mailboxes
            .contains(&MailboxId::try_from("STARRED").unwrap())
    );
    assert!(!msg.is_unread());
}

#[test]
fn messages_in_one_thread_share_a_provider_thread() {
    // A (root) and B (reply) share the same Gmail threadId → the same provider thread.
    let a = message(META);
    let b = message(META_LABELED);
    let thread = |m: &Message| m.thread.as_ref().unwrap().id.clone();
    assert_eq!(thread(&a), thread(&b));
}

#[test]
fn full_format_detects_no_attachment_on_a_plain_text_message() {
    // The `full` payload for a text/plain message has no attachment part.
    let msg = message(FULL);
    assert!(!msg.has_attachment);
    // The full format still normalizes the same envelope.
    assert_eq!(
        msg.envelope.subject.as_deref(),
        Some("Fixture: first message")
    );
}

#[test]
fn an_archived_uncategorized_message_falls_back_to_all_mail() {
    // A message whose only labels are keyword-only ones has no folder-like membership;
    // it falls back to the synthetic All Mail so the non-empty invariant holds.
    let json = serde_json::json!({
        "id": "m", "threadId": "t", "labelIds": ["STARRED"]
    });
    let msg = message_from_json(&json).unwrap();
    assert!(
        msg.mailboxes
            .contains(&MailboxId::try_from(ALL_MAIL_ID).unwrap())
    );
    assert_eq!(msg.mailboxes.len().get(), 1);
    assert!(msg.has_system_keyword(SystemKeyword::Flagged));
}

#[test]
fn a_message_with_no_labels_field_still_lands_in_all_mail() {
    let json = serde_json::json!({ "id": "m", "threadId": "t" });
    let msg = message_from_json(&json).unwrap();
    assert!(
        msg.mailboxes
            .contains(&MailboxId::try_from(ALL_MAIL_ID).unwrap())
    );
    // No UNREAD → seen.
    assert!(!msg.is_unread());
}

#[test]
fn address_parsing_handles_names_bare_addrs_and_quoted_commas() {
    let json = serde_json::json!({
        "id": "m", "threadId": "t", "labelIds": ["INBOX"],
        "payload": { "headers": [
            { "name": "From", "value": "Alice Example <alice@example.test>" },
            { "name": "To", "value": "bob@example.test, \"Carol, Jr\" <carol@example.test>" }
        ]}
    });
    let msg = message_from_json(&json).unwrap();
    assert_eq!(msg.envelope.from[0].name.as_deref(), Some("Alice Example"));
    assert_eq!(msg.envelope.from[0].email, "alice@example.test");
    // The comma inside the quoted display name does not split the address list.
    assert_eq!(msg.envelope.to.len(), 2);
    assert_eq!(msg.envelope.to[0].email, "bob@example.test");
    assert_eq!(msg.envelope.to[0].name, None);
    assert_eq!(msg.envelope.to[1].name.as_deref(), Some("Carol, Jr"));
    assert_eq!(msg.envelope.to[1].email, "carol@example.test");
}

#[test]
fn malformed_messages_are_protocol_errors_not_panics() {
    // No id.
    assert!(message_from_json(&serde_json::json!({ "threadId": "t" })).is_err());
    // No threadId.
    assert!(message_from_json(&serde_json::json!({ "id": "m" })).is_err());
    // A non-numeric internalDate surfaces as a protocol error.
    assert!(
        message_from_json(&serde_json::json!({
            "id": "m", "threadId": "t", "labelIds": ["INBOX"], "internalDate": "not-a-number"
        }))
        .is_err()
    );
}

#[test]
fn label_without_an_id_is_a_protocol_error() {
    assert!(label_from_json(&serde_json::json!({ "name": "x" })).is_err());
}

#[test]
fn a_label_carries_an_unread_count_only_where_the_api_returns_one() {
    // `users.labels.get` returns the counts…
    let detailed = label_from_json(&serde_json::json!({
        "id": "Label_1", "name": "Clients", "messagesTotal": 90, "messagesUnread": 7
    }))
    .unwrap()
    .unwrap();
    assert_eq!(detailed.unread_count, Some(7));

    // …`users.labels.list`, the call the folder sync makes, does not. Absent rather
    // than zero, so a host shows no badge instead of a wrong one.
    let listed = label_from_json(&serde_json::json!({
        "id": "Label_1", "name": "Clients", "type": "user"
    }))
    .unwrap()
    .unwrap();
    assert_eq!(listed.unread_count, None);
}
