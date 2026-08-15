//! Offline fetch/paging tests: the label list, message snapshot, history delta, and
//! raw-source fetch driven against the captured fixtures through the fixture-routing
//! fake and the reqwest replay server.

use engine_core::mail::MailboxRole;

use super::*;
use crate::test_support::{fake_client, fake_client_fallible, json, replay_server, tls};

const LABELS: &str = include_str!("../tests/fixtures/mail/labels.json");
const PROFILE: &str = include_str!("../tests/fixtures/mail/profile.json");
const LIST: &str = include_str!("../tests/fixtures/mail/messages_list.json");
const META: &str = include_str!("../tests/fixtures/mail/message_metadata.json");
const META_LABELED: &str = include_str!("../tests/fixtures/mail/message_metadata_labeled.json");
const RAW: &str = include_str!("../tests/fixtures/mail/message_raw.json");
const HISTORY: &str = include_str!("../tests/fixtures/mail/history_delta.json");
const HISTORY_DELETED: &str = include_str!("../tests/fixtures/mail/history_deleted.json");
const HISTORY_GONE: &str = include_str!("../tests/fixtures/error/history_gone.json");

/// The message-get routes both snapshot and delta re-fetch need (message-0 has no
/// captured detail fixture, so it is a minimal inline valid message).
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

#[tokio::test]
async fn labels_map_roles_and_append_all_mail() {
    let client = fake_client(vec![("/labels", json(LABELS))]);
    let mailboxes = labels(&client).await.unwrap();
    // The keyword-only labels are excluded; All Mail is appended.
    assert!(!mailboxes.iter().any(|m| m.id.as_str() == "STARRED"));
    assert!(mailboxes.iter().any(|m| m.role == Some(MailboxRole::Inbox)));
    assert!(
        mailboxes
            .iter()
            .any(|m| m.role == Some(MailboxRole::All) && m.id.as_str() == "ALL_MAIL")
    );
}

#[tokio::test]
async fn current_history_id_reads_the_profile_cursor() {
    let client = fake_client(vec![("/profile", json(PROFILE))]);
    let cursor = current_history_id(&client).await.unwrap();
    // The scrubbed profile's historyId is the snapshot's delta start cursor.
    assert!(!cursor.as_str().is_empty());
    assert!(cursor.as_str().chars().all(|c| c.is_ascii_digit()));
}

#[tokio::test]
async fn message_source_decodes_the_base64url_raw() {
    let client = fake_client(vec![("/messages/message-1", json(RAW))]);
    let key = ProviderKey::new("message-1").unwrap();
    let raw = message_source(&client, &key).await.unwrap();
    let text = String::from_utf8(raw.as_bytes().to_vec()).unwrap();
    assert!(text.contains("Subject: Fixture: first message"), "{text}");
    assert!(text.contains("The first fixture message body."));
}

#[tokio::test]
async fn snapshot_page_lists_fetches_each_and_carries_the_history_cursor() {
    let mut routes = vec![("/messages?maxResults", json(LIST))];
    routes.extend(message_routes());
    let client = fake_client(routes);
    let history = SyncState::new("1617");
    let page = snapshot_page(&client, None, None, &history).await.unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    // All three listed ids are fetched full and present; the cursor is the captured
    // account historyId, not anything the list returned.
    assert_eq!(page.changed.len(), 3);
    assert_eq!(page.present.len(), 3);
    assert!(page.removed.is_empty());
    assert_eq!(page.next_cursor.as_str(), "1617");
    assert!(page.next_page.is_none());
}

#[tokio::test]
async fn snapshot_windows_by_after_epoch_only_when_a_floor_is_set() {
    let client = fake_client(vec![]);
    let floor = CalendarDate::new(2026, 4, 1).unwrap();
    let windowed = list_url(&client, None, Some(floor));
    // Midnight 2026-04-01 UTC = 1775001600.
    assert!(windowed.contains("&q=after:1775001600"), "{windowed}");
    assert!(!list_url(&client, None, None).contains("q=after"));
}

#[tokio::test]
async fn page_urls_continue_from_a_page_token() {
    let client = fake_client(vec![]);
    let token = PageToken::new("NEXT_PAGE");
    assert!(list_url(&client, Some(&token), None).contains("&pageToken=NEXT_PAGE"));
    assert!(
        history_url(&client, &SyncState::new("42"), Some(&token)).contains("&pageToken=NEXT_PAGE")
    );
    assert!(history_url(&client, &SyncState::new("42"), None).contains("startHistoryId=42"));
}

#[tokio::test]
async fn snapshot_skips_a_message_that_404s_between_list_and_get() {
    // A message listed but deleted before its get → skipped, not fatal (a later delta
    // reports the removal). message-0 is routed to a 404.
    let client = fake_client_fallible(vec![
        ("/messages?maxResults", Ok(json(LIST))),
        ("/messages/message-2", Ok(json(META_LABELED))),
        ("/messages/message-1", Ok(json(META))),
        (
            "/messages/message-0",
            Err((404, json(r#"{"error":{"code":404}}"#))),
        ),
    ]);
    let page = snapshot_page(&client, None, None, &SyncState::new("9"))
        .await
        .unwrap();
    // Only the two fetchable messages survive.
    assert_eq!(page.changed.len(), 2);
    assert_eq!(page.present.len(), 2);
}

#[tokio::test]
async fn delta_page_refetches_changed_and_advances_the_cursor() {
    let mut routes = vec![("/history?startHistoryId", json(HISTORY))];
    routes.extend(message_routes());
    let client = fake_client(routes);
    let page = delta_page(&client, &SyncState::new("1532"), None)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    // The delta touches message-1 (added) and message-2 (label changes) → both refetched.
    assert_eq!(page.changed.len(), 2);
    assert!(page.present.is_empty()); // a delta carries no present set
    assert!(page.removed.is_empty());
    // The cursor advances to the response's latest historyId.
    assert_eq!(page.next_cursor.as_str(), "1681");
}

#[tokio::test]
async fn delta_page_tombstones_a_deleted_message_without_refetch() {
    // message-3 is added *and* deleted in the same window → tombstone only, no re-fetch
    // route provided, so a re-fetch attempt would error; the test passing proves none.
    let client = fake_client(vec![("/history?startHistoryId", json(HISTORY_DELETED))]);
    let page = delta_page(&client, &SyncState::new("1681"), None)
        .await
        .unwrap();
    assert!(page.changed.is_empty());
    assert_eq!(page.removed.len(), 1);
    assert_eq!(page.removed[0].as_str(), "message-3");
}

/// One `labelsRemoved` history page in the captured shape (see `history_delta.json`):
/// `labelIds` is what moved, `message.labelIds` what the message was left holding.
fn labels_removed(moved: &[&str], resulting: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "history": [{
            "id": "1700",
            "labelsRemoved": [{
                "labelIds": moved,
                "message": { "id": "message-9", "threadId": "t9", "labelIds": resulting },
            }],
        }],
        "historyId": "1700",
    })
}

#[tokio::test]
async fn a_mark_read_is_answered_by_the_history_page_itself() {
    // `UNREAD` removed and nothing else: the page already carries the resulting label
    // set, so this costs no `messages.get` at all. No message route is registered, so a
    // re-fetch would error — the test passing is the proof.
    let client = fake_client(vec![(
        "/history?startHistoryId",
        labels_removed(&["UNREAD"], &["INBOX", "CATEGORY_PERSONAL"]),
    )]);
    let page = delta_page(&client, &SyncState::new("1681"), None)
        .await
        .unwrap();

    assert!(page.changed.is_empty(), "a mark-read rewrites no message");
    assert_eq!(page.patched.len(), 1);
    assert_eq!(page.patched[0].key.as_str(), "message-9");
    // Gmail's `UNREAD` is inverted, so its *absence* from the resulting set is `$seen`.
    assert!(
        page.patched[0]
            .state
            .keywords
            .iter()
            .any(|k| k.as_system() == Some(engine_core::mail::SystemKeyword::Seen))
    );
}

#[tokio::test]
async fn an_archive_is_a_move_and_still_refetches() {
    // Gmail files by label, so removing `INBOX` is an archive — a membership change,
    // which a state change does not carry. It must stay a whole object.
    let client = fake_client(vec![
        (
            "/history?startHistoryId",
            labels_removed(&["INBOX"], &["UNREAD"]),
        ),
        (
            "/messages/message-9",
            serde_json::json!({ "id": "message-9", "threadId": "t9", "labelIds": ["UNREAD"] }),
        ),
    ]);
    let page = delta_page(&client, &SyncState::new("1681"), None)
        .await
        .unwrap();

    assert!(
        page.patched.is_empty(),
        "a move is not a state change: it would lose the archive"
    );
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.changed[0].id.as_str(), "message-9");
}

#[tokio::test]
async fn an_absent_resulting_label_set_refetches_rather_than_guessing() {
    // A partial with no `message.labelIds` at all is not an empty label set. Reading it
    // as one would hand `keywords_from_labels` an empty list, whose missing `UNREAD`
    // means `$seen` — silently marking unread mail read.
    let client = fake_client(vec![
        (
            "/history?startHistoryId",
            serde_json::json!({
                "history": [{
                    "id": "1700",
                    "labelsRemoved": [{
                        "labelIds": ["UNREAD"],
                        "message": { "id": "message-9", "threadId": "t9" },
                    }],
                }],
                "historyId": "1700",
            }),
        ),
        (
            "/messages/message-9",
            serde_json::json!({ "id": "message-9", "threadId": "t9", "labelIds": ["INBOX"] }),
        ),
    ]);
    let page = delta_page(&client, &SyncState::new("1681"), None)
        .await
        .unwrap();

    assert!(page.patched.is_empty());
    assert_eq!(page.changed.len(), 1);
}

#[tokio::test]
async fn a_new_message_that_also_changed_labels_is_still_fetched_whole() {
    // The same id in `messagesAdded` and `labelsAdded`: we hold none of its content, so
    // the label half must not shortcut the fetch.
    let client = fake_client(vec![
        (
            "/history?startHistoryId",
            serde_json::json!({
                "history": [{
                    "id": "1700",
                    "messagesAdded": [{
                        "message": { "id": "message-9", "threadId": "t9", "labelIds": ["UNREAD"] },
                    }],
                    "labelsAdded": [{
                        "labelIds": ["STARRED"],
                        "message": {
                            "id": "message-9", "threadId": "t9",
                            "labelIds": ["UNREAD", "STARRED"],
                        },
                    }],
                }],
                "historyId": "1700",
            }),
        ),
        (
            "/messages/message-9",
            serde_json::json!({
                "id": "message-9", "threadId": "t9", "labelIds": ["UNREAD", "STARRED"],
            }),
        ),
    ]);
    let page = delta_page(&client, &SyncState::new("1681"), None)
        .await
        .unwrap();

    assert!(page.patched.is_empty());
    assert_eq!(page.changed.len(), 1);
}

#[tokio::test]
async fn delta_page_maps_a_404_to_history_expired() {
    // An aged-out startHistoryId (404) becomes the resync signal, not a plain permanent.
    let client = fake_client_fallible(vec![(
        "/history?startHistoryId",
        Err((404, json(HISTORY_GONE))),
    )]);
    let err = delta_page(&client, &SyncState::new("1"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, GoogleError::HistoryExpired(_)));
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::NeedsResync
    );
}

#[tokio::test]
async fn the_whole_stack_runs_over_the_reqwest_replay_server() {
    // Drive the real reqwest transport (via with_base) at the replay server, proving the
    // label fetch works end-to-end without a live token.
    let base = replay_server(vec![("/labels", json(LABELS))]);
    let client = crate::GoogleClient::with_base("t", base, tls()).unwrap();
    let mailboxes = labels(&client).await.unwrap();
    assert!(mailboxes.iter().any(|m| m.role == Some(MailboxRole::Inbox)));
}
