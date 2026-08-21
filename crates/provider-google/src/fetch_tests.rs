//! Offline fetch/paging tests: the label list, message snapshot, history delta, and
//! raw-source fetch driven against the captured fixtures through the fixture-routing
//! fake and the reqwest replay server.

use engine_core::{ids::MailboxId, mail::MailboxRole};

use super::*;
use crate::test_support::{
    FakeRoute, fake_client, fake_client_fallible, json, probe_client, replay_server, retry, tls,
};

const LABELS: &str = include_str!("../tests/fixtures/mail/labels.json");
const PROFILE: &str = include_str!("../tests/fixtures/mail/profile.json");
const LIST: &str = include_str!("../tests/fixtures/mail/messages_list.json");
const META: &str = include_str!("../tests/fixtures/mail/message_metadata.json");
const META_LABELED: &str = include_str!("../tests/fixtures/mail/message_metadata_labeled.json");
const RAW: &str = include_str!("../tests/fixtures/mail/message_raw.json");
const LIST_JUNK: &str = include_str!("../tests/fixtures/mail/messages_list_junk.json");
const META_JUNK: &str = include_str!("../tests/fixtures/mail/message_metadata_junk.json");
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
async fn a_junk_message_is_filed_in_spam_and_kept_in_the_present_set() {
    // Captured from a real account whose message sat in Junk. `present` is what a
    // snapshot tombstones against, so a spam message missing from it is not merely
    // unreported — it is deleted from the store on the next reconcile.
    let client = fake_client(vec![
        ("/messages?maxResults", json(LIST_JUNK)),
        ("/messages/message-junk", json(META_JUNK)),
    ]);
    let page = snapshot_page(&client, None, None, &SyncState::new("1662"))
        .await
        .unwrap();

    let message = page.changed.first().expect("the junk message is carried");
    assert!(
        page.present.contains(message.id.key()),
        "a junk message stays in the present set"
    );
    // `SPAM` is an ordinary place label, so it files like any other folder; `UNREAD`
    // is a keyword and does not.
    let places: Vec<&str> = message.mailboxes.iter().map(MailboxId::as_str).collect();
    assert!(places.contains(&"SPAM"), "{places:?}");
    assert!(!places.contains(&"UNREAD"), "{places:?}");
}

#[tokio::test]
async fn every_snapshot_page_asks_for_spam_and_trash() {
    // `messages.list` omits SPAM and TRASH unless asked, but `history.list` reports
    // their label changes regardless — so without this flag a delta files a message
    // into Junk and the next snapshot tombstones it, and which one the store believes
    // depends on whether the last pass happened to be a snapshot.
    let client = fake_client(vec![]);
    let floor = CalendarDate::new(2026, 4, 1).unwrap();
    let token = PageToken::new("NEXT_PAGE");
    for url in [
        list_url(&client, None, None),
        list_url(&client, None, Some(floor)),
        list_url(&client, Some(&token), Some(floor)),
    ] {
        assert!(url.contains("&includeSpamTrash=true"), "{url}");
    }
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

/// A list page of `count` ids plus a get route per id, for the fan-out tests. Ids are
/// fixed-width because the fake routes on substring and `msg-1` would shadow `msg-10`.
fn wide_page(count: usize) -> (Vec<(String, FakeRoute)>, Vec<String>) {
    let ids: Vec<String> = (0..count).map(|i| format!("msg-{i:03}")).collect();
    let list = serde_json::json!({
        "messages": ids
            .iter()
            .map(|id| serde_json::json!({ "id": id, "threadId": id }))
            .collect::<Vec<_>>(),
    });
    let mut routes = vec![("/messages?maxResults".to_owned(), Ok(list))];
    for id in &ids {
        routes.push((
            format!("/messages/{id}"),
            Ok(serde_json::json!({ "id": id, "threadId": id, "labelIds": ["INBOX"] })),
        ));
    }
    (routes, ids)
}

/// Borrows owned routes into the `&str`-keyed shape the fake builders take.
fn as_routes(routes: &[(String, FakeRoute)]) -> Vec<(&str, FakeRoute)> {
    routes
        .iter()
        .map(|(key, answer)| (key.as_str(), answer.clone()))
        .collect()
}

#[tokio::test]
async fn a_snapshot_page_fetches_its_messages_concurrently() {
    // The defect this locks out is a page drained one message at a time: every id costs a
    // round trip Gmail cannot batch, so a serial page is as deep as it is wide and the first
    // rows reach the list only after the last fetch has returned.
    let (routes, _) = wide_page(60);
    let (client, probe) = probe_client(as_routes(&routes));
    let page = snapshot_page(&client, None, None, &SyncState::new("7"))
        .await
        .unwrap();
    assert_eq!(page.changed.len(), 60);
    // One list call plus one get per message.
    assert_eq!(probe.calls(), 61);
    assert_eq!(
        probe.peak(),
        MAX_CONCURRENT_GETS,
        "a wide page should fill the fetch window; a serial drain peaks at 1"
    );
}

#[tokio::test]
async fn a_page_smaller_than_the_window_never_exceeds_its_own_size() {
    // The window is a ceiling, not a target: a page of three does not open twenty requests.
    let (routes, _) = wide_page(3);
    let (client, probe) = probe_client(as_routes(&routes));
    snapshot_page(&client, None, None, &SyncState::new("7"))
        .await
        .unwrap();
    assert_eq!(probe.peak(), 3);
}

#[tokio::test]
async fn concurrent_fetches_still_land_in_the_order_the_server_listed_them() {
    // Fetches complete in whatever order they finish; the page must not inherit that order.
    let (routes, ids) = wide_page(40);
    let (client, _) = probe_client(as_routes(&routes));
    let page = snapshot_page(&client, None, None, &SyncState::new("7"))
        .await
        .unwrap();
    let got: Vec<&str> = page.changed.iter().map(|m| m.id.key().as_str()).collect();
    assert_eq!(got, ids.iter().map(String::as_str).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_delta_page_refetches_its_new_arrivals_concurrently() {
    // The delta's re-fetch is the same shape as the snapshot's and pays the same cost: a
    // `messagesAdded` record carries no subject, sender or body, so each one is a round trip.
    let history: Vec<serde_json::Value> = (0..30)
        .map(|i| {
            serde_json::json!({
                "messagesAdded": [{ "message": { "id": format!("msg-{i:03}") } }]
            })
        })
        .collect();
    let mut routes = vec![(
        "/history?".to_owned(),
        Ok(serde_json::json!({ "history": history, "historyId": "99" })),
    )];
    for i in 0..30 {
        let id = format!("msg-{i:03}");
        routes.push((
            format!("/messages/{id}"),
            Ok(serde_json::json!({ "id": id, "threadId": id, "labelIds": ["INBOX"] })),
        ));
    }
    let (client, probe) = probe_client(as_routes(&routes));
    let page = delta_page(&client, &SyncState::new("1"), None)
        .await
        .unwrap();
    assert_eq!(page.changed.len(), 30);
    assert_eq!(probe.peak(), MAX_CONCURRENT_GETS);
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
    // message-1 was added, so it is fetched whole; message-2 only changed labels, which the
    // history page already answered in full.
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.patched.len(), 1);
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
async fn an_archive_is_a_state_change_that_carries_the_new_filing() {
    // Gmail files by label, so removing `INBOX` is an archive — a membership change. A state
    // change carries filing, so this needs no re-fetch either; what it must not do is lose the
    // move. No message route is registered, so a re-fetch would error.
    let client = fake_client(vec![(
        "/history?startHistoryId",
        labels_removed(&["INBOX"], &["UNREAD", "CATEGORY_PERSONAL"]),
    )]);
    let page = delta_page(&client, &SyncState::new("1681"), None)
        .await
        .unwrap();

    assert!(page.changed.is_empty(), "an archive rewrites no message");
    assert_eq!(page.patched.len(), 1);
    let filing = page.patched[0]
        .state
        .mailboxes
        .as_ref()
        .expect("Gmail files in place, so the change says where");
    assert!(!filing.contains(&MailboxId::try_from("INBOX").unwrap()));
    assert!(filing.contains(&MailboxId::try_from("CATEGORY_PERSONAL").unwrap()));
    // `UNREAD` is keyword state, never a place.
    assert!(!filing.contains(&MailboxId::try_from("UNREAD").unwrap()));
}

#[tokio::test]
async fn a_message_left_with_no_folder_label_is_filed_in_all_mail() {
    // An archived, uncategorized message carries no folder-like label at all, and the engine's
    // membership set may not be empty — the same synthetic home the whole-object path uses.
    let client = fake_client(vec![(
        "/history?startHistoryId",
        labels_removed(&["INBOX"], &[]),
    )]);
    let page = delta_page(&client, &SyncState::new("1681"), None)
        .await
        .unwrap();
    let filing = page.patched[0].state.mailboxes.as_ref().expect("filing");
    assert!(filing.contains(&MailboxId::try_from("ALL_MAIL").unwrap()));
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
    let client = crate::GoogleClient::with_base("t", base, tls(), retry()).unwrap();
    let mailboxes = labels(&client).await.unwrap();
    assert!(mailboxes.iter().any(|m| m.role == Some(MailboxRole::Inbox)));
}
