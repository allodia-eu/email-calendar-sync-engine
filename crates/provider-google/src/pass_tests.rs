//! The passes a Gmail message sync runs, by where its cursor stands: a backfill fetched a page
//! at a time and checkpointed after each, mail arriving meanwhile, a backfill picked up where it
//! stopped, and the reconcile that ends it.

use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::{SyncState, SyncWindow},
};
use engine_provider::{EmailChunk, PassMode, Provider};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    GmailProvider,
    test_support::{FakeRoute, fake_client_fallible, json as fixture, recording_client},
};

const LIST: &str = include_str!("../tests/fixtures/mail/messages_list.json");
const META: &str = include_str!("../tests/fixtures/mail/message_metadata.json");
const META_LABELED: &str = include_str!("../tests/fixtures/mail/message_metadata_labeled.json");
const PROFILE: &str = include_str!("../tests/fixtures/mail/profile.json");
const HISTORY: &str = include_str!("../tests/fixtures/mail/history_delta.json");
const HISTORY_GONE: &str = include_str!("../tests/fixtures/error/history_gone.json");
const INVALID_PAGE_TOKEN: &str = include_str!("../tests/fixtures/error/invalid_page_token.json");

/// The profile fixture's historyId, where a backfill from the top starts.
const PROFILE_HISTORY: &str = "1988";

/// A history page reporting nothing since the cursor.
fn quiet_history() -> Value {
    json!({ "historyId": PROFILE_HISTORY })
}

/// The listing split in two pages, the first naming the second.
fn first_page() -> Value {
    json!({ "messages": [{ "id": "message-2", "threadId": "message-1" }], "nextPageToken": "page-2" })
}

fn second_page() -> Value {
    json!({ "messages": [
        { "id": "message-1", "threadId": "message-1" },
        { "id": "message-0", "threadId": "message-0" }
    ] })
}

fn messages() -> Vec<(&'static str, FakeRoute)> {
    vec![
        ("/messages/message-2", Ok(fixture(META_LABELED))),
        ("/messages/message-1", Ok(fixture(META))),
        (
            "/messages/message-0",
            Ok(json!({ "id": "message-0", "threadId": "message-0", "labelIds": ["INBOX"] })),
        ),
    ]
}

/// The two-page listing, page 2 first because the fake answers the first key a URL contains.
fn two_pages() -> Vec<(&'static str, FakeRoute)> {
    vec![
        ("pageToken=page-2", Ok(second_page())),
        ("/messages?maxResults", Ok(first_page())),
    ]
}

/// Every chunk of one pass from `cursor`.
async fn pass(provider: &GmailProvider, cursor: Option<&str>) -> Vec<EmailChunk> {
    let account = AccountId::try_from("acct-1").unwrap();
    let cursor = cursor.map(SyncState::new);
    provider
        .stream_email(&account, cursor.as_ref(), SyncWindow::full(), 0, 0)
        .map(|chunk| chunk.expect("a chunk"))
        .collect()
        .await
}

fn fetched(chunks: &[EmailChunk]) -> Vec<&str> {
    chunks
        .iter()
        .flat_map(|chunk| &chunk.changed)
        .map(|message| message.id.key().as_str())
        .collect()
}

fn checkpoints(chunks: &[EmailChunk]) -> Vec<&str> {
    chunks
        .iter()
        .filter_map(|chunk| chunk.advance_to.as_ref())
        .map(SyncState::as_str)
        .collect()
}

fn routed(routes: &[Vec<(&'static str, FakeRoute)>]) -> GmailProvider {
    GmailProvider::new(fake_client_fallible(routes.concat()))
}

#[tokio::test]
async fn a_first_sync_fetches_the_window_and_leaves_the_reconcile_to_the_next_pass() {
    let provider = routed(&[
        vec![
            ("/profile", Ok(fixture(PROFILE))),
            ("/history?startHistoryId", Ok(quiet_history())),
            ("/messages?maxResults", Ok(fixture(LIST))),
        ],
        messages(),
    ]);

    let chunks = pass(&provider, None).await;

    assert_eq!(fetched(&chunks), ["message-2", "message-1", "message-0"]);
    assert!(chunks.iter().all(|chunk| chunk.mode == PassMode::Additive));
    assert_eq!(checkpoints(&chunks).last(), Some(&"reconcile;h=1988"));
}

#[tokio::test]
async fn each_page_is_checkpointed_with_the_token_of_the_next() {
    // A pass killed after page one resumes at page two rather than paying for page one again.
    let provider = routed(&[
        vec![
            ("/profile", Ok(fixture(PROFILE))),
            ("/history?startHistoryId", Ok(quiet_history())),
        ],
        two_pages(),
        messages(),
    ]);

    let chunks = pass(&provider, None).await;

    assert_eq!(
        checkpoints(&chunks),
        ["backfill;h=1988;p=page-2", "reconcile;h=1988"]
    );
    let after_page_one = chunks
        .iter()
        .position(|chunk| chunk.advance_to.is_some())
        .unwrap();
    assert_eq!(fetched(&chunks[..after_page_one]), ["message-2"]);
}

#[tokio::test]
async fn a_backfill_picks_up_at_its_page_token() {
    let mut routes = vec![("/history?startHistoryId", Ok(quiet_history()))];
    routes.extend(two_pages());
    routes.extend(messages());
    let (client, log) = recording_client(routes);
    let provider = GmailProvider::new(client);

    let chunks = pass(&provider, Some("backfill;h=1988;p=page-2")).await;

    assert_eq!(fetched(&chunks), ["message-1", "message-0"]);
    let requests = log.lock().unwrap();
    assert!(
        requests
            .iter()
            .filter(|r| r.url.contains("/messages?"))
            .all(|r| r.url.contains("pageToken=page-2")),
        "the listing started over: {requests:?}"
    );
    assert!(!requests.iter().any(|r| r.url.contains("/profile")));
}

#[tokio::test]
async fn mail_arriving_during_a_backfill_is_fetched_before_the_next_page() {
    // A backfill can run for hours; the inbox must not stand still while it does.
    let provider = routed(&[
        vec![
            ("/history?startHistoryId", Ok(fixture(HISTORY))),
            ("/messages?maxResults", Ok(fixture(LIST))),
        ],
        messages(),
    ]);

    let chunks = pass(&provider, Some("backfill;h=1532;p=")).await;

    assert_eq!(
        fetched(&chunks),
        ["message-1", "message-2", "message-1", "message-0"]
    );
    assert_eq!(checkpoints(&chunks).last(), Some(&"reconcile;h=1681"));
}

#[tokio::test]
async fn an_expired_history_starts_the_backfill_over_from_the_top() {
    // What changed since the cursor can no longer be known, so the window is fetched again and
    // the reconcile at its end removes what is gone.
    let provider = routed(&[
        vec![
            // The fresh cursor before the stale one, which is a prefix of it.
            ("/history?startHistoryId=1988", Ok(quiet_history())),
            (
                "/history?startHistoryId=1",
                Err((404, fixture(HISTORY_GONE))),
            ),
            ("/profile", Ok(fixture(PROFILE))),
        ],
        two_pages(),
        messages(),
    ]);

    let chunks = pass(&provider, Some("backfill;h=1;p=page-2")).await;

    assert_eq!(fetched(&chunks), ["message-2", "message-1", "message-0"]);
    assert_eq!(checkpoints(&chunks).last(), Some(&"reconcile;h=1988"));
}

#[tokio::test]
async fn a_page_token_gmail_no_longer_reads_starts_the_backfill_over() {
    let provider = routed(&[
        vec![
            ("/history?startHistoryId", Ok(quiet_history())),
            ("pageToken=stale", Err((400, fixture(INVALID_PAGE_TOKEN)))),
            ("/messages?maxResults", Ok(fixture(LIST))),
        ],
        messages(),
    ]);

    let chunks = pass(&provider, Some("backfill;h=1988;p=stale")).await;

    assert_eq!(fetched(&chunks), ["message-2", "message-1", "message-0"]);
    assert_eq!(checkpoints(&chunks).last(), Some(&"reconcile;h=1988"));
}

#[tokio::test]
async fn the_reconcile_lists_the_window_without_fetching_a_message() {
    // Listing costs 5 units for up to 500 ids; fetching costs 20 a message. The reconcile needs
    // only the ids, so it never pays for a message.
    let (client, log) = recording_client(vec![("/messages?maxResults", Ok(fixture(LIST)))]);
    let provider = GmailProvider::new(client);

    let chunks = pass(&provider, Some("reconcile;h=1681")).await;

    assert_eq!(fetched(&chunks), [] as [&str; 0]);
    assert!(chunks.iter().all(|chunk| chunk.mode == PassMode::Reconcile));
    let present: Vec<&str> = chunks
        .iter()
        .flat_map(|chunk| &chunk.present)
        .map(ProviderKey::as_str)
        .collect();
    assert_eq!(present, ["message-2", "message-1", "message-0"]);
    assert_eq!(checkpoints(&chunks), ["1681"]);
    let requests = log.lock().unwrap();
    assert!(
        requests.iter().all(|r| r.url.contains("maxResults=500")),
        "{requests:?}"
    );
}

#[tokio::test]
async fn a_synced_cursor_runs_a_delta_and_nothing_more() {
    let provider = routed(&[
        vec![("/history?startHistoryId", Ok(fixture(HISTORY)))],
        messages(),
    ]);

    let chunks = pass(&provider, Some("1532")).await;

    assert_eq!(fetched(&chunks), ["message-1"]);
    assert_eq!(checkpoints(&chunks), ["1681"]);
}

#[tokio::test]
async fn a_fresh_backfill_asks_for_no_history_before_its_first_page() {
    // The historyId was captured a moment before; a delta from it can only be empty.
    let mut routes = vec![
        ("/profile", Ok(fixture(PROFILE))),
        ("/messages?maxResults", Ok(fixture(LIST))),
    ];
    routes.extend(messages());
    let (client, log) = recording_client(routes);
    let provider = GmailProvider::new(client);

    let chunks = pass(&provider, None).await;

    assert_eq!(fetched(&chunks), ["message-2", "message-1", "message-0"]);
    let requests = log.lock().unwrap();
    assert!(
        !requests.iter().any(|r| r.url.contains("/history")),
        "{requests:?}"
    );
}
