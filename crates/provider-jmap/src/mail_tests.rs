use engine_core::mail::SystemKeyword;

use super::*;

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
    assert_ne!(mailbox_by_role(&MailboxRole::Inbox).as_str(), "");
    assert!(all.iter().any(|m| m.name == "Archive" && m.role.is_none()));
    assert!(all.iter().any(|m| m.name == "Projects" && m.role.is_none()));
    assert!(all.iter().any(|m| m.role == Some(MailboxRole::Sent)));
    assert!(all.iter().any(|m| m.role == Some(MailboxRole::Trash)));
}

#[test]
fn a_mailbox_whose_items_cannot_be_read_is_not_selectable() {
    // Every captured mailbox grants `mayReadItems`.
    assert!(mailboxes().iter().all(|m| m.selectable));

    let refused = serde_json::json!({ "id": "m", "myRights": { "mayReadItems": false } });
    assert!(!mailbox_from_json(&refused).unwrap().selectable);
    // No rights reported says nothing against opening it.
    let silent = serde_json::json!({ "id": "m" });
    assert!(mailbox_from_json(&silent).unwrap().selectable);
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
    assert_eq!(no_msgid.envelope.message_id, []);
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
    assert_eq!(baseline.envelope.cc, []);
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
fn mailbox_unread_count_reads_emails_not_threads() {
    let counted = mailbox_from_json(&serde_json::json!({
        "id": "inbox", "name": "Inbox", "role": "inbox",
        "totalEmails": 1200, "unreadEmails": 545,
        "totalThreads": 900, "unreadThreads": 401
    }))
    .unwrap();
    assert_eq!(counted.unread_count, Some(545));

    // A server that reports nothing leaves it absent — a host must be able to
    // tell "nothing unread" from "nobody counted".
    let silent = mailbox_from_json(&serde_json::json!({ "id": "inbox", "name": "Inbox" })).unwrap();
    assert_eq!(silent.unread_count, None);

    // Zero is a real answer and survives as one.
    let read = mailbox_from_json(
        &serde_json::json!({ "id": "inbox", "name": "Inbox", "unreadEmails": 0 }),
    )
    .unwrap();
    assert_eq!(read.unread_count, Some(0));
}

#[test]
fn mailbox_accepts_children_as_its_rights_say() {
    let accepts = |rights: Value| {
        mailbox_from_json(&serde_json::json!({ "id": "a", "name": "A", "myRights": rights }))
            .unwrap()
            .accepts_children
    };
    assert!(accepts(serde_json::json!({ "mayCreateChild": true })));
    // A shared mailbox the user may read but not build in.
    assert!(!accepts(serde_json::json!({ "mayCreateChild": false })));
    // Rights not reported: nothing is withheld, and the server still answers.
    assert!(accepts(Value::Null));
}

#[test]
fn sent_at_offset_normalizes_and_bad_values_are_non_fatal() {
    // RFC 8621 §4.1.1 `sentAt` is a `Date` (RFC 8620 §1.4): a numeric offset
    // is legal and must normalize to UTC, not abort the sync (issue #38).
    let offset = message_from_json(&serde_json::json!({
        "id": "m1", "mailboxIds": { "inbox": true },
        "sentAt": "2026-07-05T14:13:58+02:00",
    }))
    .unwrap();
    assert_eq!(offset.sent_at.unwrap().to_string(), "2026-07-05T12:13:58Z");

    // A single unparseable `sentAt` degrades that one field to `None`; the
    // message (and thus the mailbox sync) still succeeds (issue #38).
    let malformed = message_from_json(&serde_json::json!({
        "id": "m2", "mailboxIds": { "inbox": true },
        "sentAt": "not-a-date", "receivedAt": "2026-07-05T10:00:00Z",
    }))
    .unwrap();
    assert!(malformed.sent_at.is_none());
    assert!(malformed.received_at.is_some());

    // An absent `sentAt` is `None`, unchanged.
    let absent = message_from_json(&serde_json::json!({
        "id": "m3", "mailboxIds": { "inbox": true },
    }))
    .unwrap();
    assert!(absent.sent_at.is_none());
}

#[test]
fn malformed_email_is_a_protocol_error_not_a_panic() {
    // No id.
    assert!(message_from_json(&serde_json::json!({ "mailboxIds": { "a": true } })).is_err());
    // Empty mailboxIds violates RFC 8621 §4.1.1.
    assert!(message_from_json(&serde_json::json!({ "id": "x", "mailboxIds": {} })).is_err());
}
