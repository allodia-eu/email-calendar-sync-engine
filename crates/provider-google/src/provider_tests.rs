//! Offline provider-orchestration tests: `sync_mailboxes`, the snapshot/history-delta
//! `sync_email` drain, the 404-restart, and message-source fetch — all against the
//! captured fixtures through the fixture-routing fake.

use engine_core::{
    ids::{MailboxId, MessageId},
    mail::{MailboxRole, Message},
    membership::Memberships,
    sync::SyncUpdate,
};

use super::*;
use crate::test_support::{fake_client, fake_client_fallible, json};

const LABELS: &str = include_str!("../tests/fixtures/mail/labels.json");
const PROFILE: &str = include_str!("../tests/fixtures/mail/profile.json");
const LIST: &str = include_str!("../tests/fixtures/mail/messages_list.json");
const META: &str = include_str!("../tests/fixtures/mail/message_metadata.json");
const META_LABELED: &str = include_str!("../tests/fixtures/mail/message_metadata_labeled.json");
const RAW: &str = include_str!("../tests/fixtures/mail/message_raw.json");
const HISTORY: &str = include_str!("../tests/fixtures/mail/history_delta.json");
const HISTORY_GONE: &str = include_str!("../tests/fixtures/error/history_gone.json");

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn message_routes() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("/messages/message-2", json(META_LABELED)),
        ("/messages/message-1", json(META)),
        (
            "/messages/message-0",
            serde_json::json!({ "id": "message-0", "threadId": "message-0", "labelIds": ["INBOX"] }),
        ),
    ]
}

#[test]
fn scopes_are_account_global_for_mail_and_a_label_container() {
    let provider = GmailProvider::new(fake_client(vec![]));
    assert_eq!(
        provider.email_scope(&account()),
        SyncScope::GmailMessages { account: account() }
    );
    assert_eq!(
        provider.mailbox_scope(&account()),
        SyncScope::GmailLabelList { account: account() }
    );
    // Mail read + on-demand source + writes + submission.
    let caps = provider.connection_info().capabilities;
    assert!(caps.mail() && caps.message_source());
    assert!(caps.submission() && caps.mail_writes());
    // The fake transport reports HTTP/2, so connection_info surfaces it.
    assert!(provider.connection_info().http_version.is_some());
}

#[tokio::test]
async fn sync_mailboxes_snapshots_labels_with_roles_and_all_mail() {
    let provider = GmailProvider::new(fake_client(vec![("/labels", json(LABELS))]));
    let sync = provider.sync_mailboxes(&account(), None).await.unwrap();
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("expected a label snapshot");
    };
    let roles: std::collections::BTreeSet<MailboxRole> =
        objects.iter().filter_map(|m| m.role.clone()).collect();
    assert!(roles.contains(&MailboxRole::Inbox));
    assert!(roles.contains(&MailboxRole::All)); // the synthetic All Mail
    // Present set covers every emitted label (full snapshot each pass).
    assert_eq!(present.len(), objects.len());
}

#[tokio::test]
async fn sync_email_snapshot_reconciles_and_persists_the_history_cursor() {
    let mut routes = vec![
        ("/profile", json(PROFILE)),
        ("/messages?maxResults", json(LIST)),
    ];
    routes.extend(message_routes());
    let provider = GmailProvider::new(fake_client(routes));
    let sync = provider.sync_email(&account(), None).await.unwrap();
    // A first sync is a reconciling snapshot with the whole present set.
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("expected a message snapshot");
    };
    assert_eq!(objects.len(), 3);
    assert_eq!(present.len(), 3);
    // The persisted cursor is the account historyId captured from the profile.
    assert!(
        sync.next_cursor
            .as_str()
            .chars()
            .all(|c| c.is_ascii_digit())
    );
}

#[tokio::test]
async fn sync_email_delta_is_additive_and_advances_the_cursor() {
    let mut routes = vec![("/history?startHistoryId", json(HISTORY))];
    routes.extend(message_routes());
    let provider = GmailProvider::new(fake_client(routes));
    let sync = provider
        .sync_email(&account(), Some(&SyncState::new("1532")))
        .await
        .unwrap();
    // A delta is additive: changed messages + explicit removals, no present set.
    assert!(!sync.is_snapshot());
    let SyncUpdate::Delta { changed, removed } = &sync.update else {
        panic!("expected a delta");
    };
    assert_eq!(changed.len(), 2); // message-1 (added) + message-2 (labels)
    assert!(removed.is_empty());
    assert_eq!(sync.next_cursor.as_str(), "1681");
}

#[tokio::test]
async fn sync_email_restarts_as_a_snapshot_when_the_history_cursor_expired() {
    // The stored historyId aged out (404 → HistoryExpired); the stream drops it and
    // restarts as a full snapshot from the profile cursor.
    let mut routes: Vec<(&str, crate::test_support::FakeRoute)> =
        vec![("/history?startHistoryId", Err((404, json(HISTORY_GONE))))];
    routes.push(("/profile", Ok(json(PROFILE))));
    routes.push(("/messages?maxResults", Ok(json(LIST))));
    for (k, v) in message_routes() {
        routes.push((k, Ok(v)));
    }
    let provider = GmailProvider::new(fake_client_fallible(routes));
    let sync = provider
        .sync_email(&account(), Some(&SyncState::new("1")))
        .await
        .unwrap();
    // Recovery yields a fresh snapshot, not an error.
    assert!(sync.is_snapshot());
    assert!(
        sync.next_cursor
            .as_str()
            .chars()
            .all(|c| c.is_ascii_digit())
    );
}

#[test]
fn with_since_sets_the_default_sync_window_and_debug_hides_state() {
    use engine_core::time::CalendarDate;
    let since = CalendarDate::new(2026, 4, 1).unwrap();
    let provider = GmailProvider::new(fake_client(vec![])).with_since(since);
    assert_eq!(provider.default_sync_window().floor(), Some(since));
    // A plain provider syncs the whole account.
    assert_eq!(
        GmailProvider::new(fake_client(vec![]))
            .default_sync_window()
            .floor(),
        None
    );
    // Debug is finite and does not leak internals.
    assert!(format!("{provider:?}").contains("GmailProvider"));
}

#[tokio::test]
async fn edit_mail_and_submit_email_route_through_the_provider() {
    // The provider's write wrappers delegate to mutate/submit; drive both through it.
    let provider = GmailProvider::new(fake_client(vec![
        ("/messages/message-1/modify", json(r#"{"id":"message-1"}"#)),
        (
            "/messages/send",
            serde_json::json!({ "id": "19f7sent0000abcd", "threadId": "19f7sent0000abcd" }),
        ),
    ]));
    let receipt = provider
        .edit_mail(
            &account(),
            &engine_provider::MailEdit::mark_seen(
                engine_core::ids::ProviderKey::new("message-1").unwrap(),
                true,
            ),
        )
        .await
        .unwrap();
    assert_eq!(receipt.message_key.as_str(), "message-1");

    let draft = engine_provider::Draft::new(
        engine_core::ids::MessageIdHeader::new("send-via-provider@test.local").unwrap(),
        engine_core::mail::EmailAddress::new("testuser@example.test"),
        vec![engine_core::mail::EmailAddress::new(
            "testuser@example.test",
        )],
        "Subject",
        "Body",
    );
    let sent = provider.submit_email(&account(), &draft).await.unwrap();
    assert_eq!(sent.email_key.as_str(), "19f7sent0000abcd");
}

#[tokio::test]
async fn fetch_message_source_fetches_and_decodes_the_raw() {
    let provider = GmailProvider::new(fake_client(vec![("/messages/message-1", json(RAW))]));
    let message = Message::new(
        MessageId::try_from("message-1").unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    );
    let raw = provider
        .fetch_message_source(&account(), &message)
        .await
        .unwrap();
    let text = String::from_utf8(raw.as_bytes().to_vec()).unwrap();
    assert!(text.contains("Fixture: first message"));
}
