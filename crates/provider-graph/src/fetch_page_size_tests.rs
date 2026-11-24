//! The page size a `messages/delta` pass asks for: `fetch_batch` as `odata.maxpagesize`, on
//! the first page, on every `@odata.nextLink` continuation and on a stored deltaLink, since the
//! header does not travel inside the link the server hands back.

use engine_core::{
    ids::{AccountId, MailboxId},
    sync::{MailboxWindows, SyncState},
};
use engine_provider::Provider;
use futures_util::StreamExt;

use super::{MAX_DELTA_PAGE, messages_page};
use crate::{
    GraphProvider,
    test_support::{FakeRoute, PreferLog, fake_client_recording, json},
};

const SNAPSHOT: &str = include_str!("../tests/fixtures/mail/messages_delta_snapshot.json");

fn inbox() -> MailboxId {
    MailboxId::try_from("folder-inbox").unwrap()
}

/// The `Prefer` values the fake was asked for, in order (empty where none was sent).
fn asked(prefers: &PreferLog) -> Vec<String> {
    prefers
        .lock()
        .unwrap()
        .iter()
        .map(|(_, prefer)| prefer.clone().unwrap_or_default())
        .collect()
}

fn size(n: usize) -> String {
    format!("odata.maxpagesize={n}")
}

/// A two-page snapshot: a first page whose `@odata.nextLink` names the second, which ends the
/// pass at its `@odata.deltaLink`.
fn two_pages() -> Vec<(&'static str, FakeRoute)> {
    let page = |id: &str, link_key: &str, link: &str| {
        serde_json::json!({
            "value": [{ "id": id, "parentFolderId": "folder-inbox", "@odata.etag": "e" }],
            link_key: link,
        })
    };
    vec![
        (
            "messages/delta?",
            Ok(page(
                "m1",
                "@odata.nextLink",
                "https://graph.microsoft.com/v1.0/me/mailFolders/folder-inbox/messages/delta-page-2",
            )),
        ),
        (
            "delta-page-2",
            Ok(page(
                "m2",
                "@odata.deltaLink",
                "https://graph.microsoft.com/v1.0/me/mailFolders/folder-inbox/messages/delta-token-1",
            )),
        ),
    ]
}

#[tokio::test]
async fn every_page_of_a_snapshot_asks_for_the_batch() {
    let (client, prefers) = fake_client_recording(two_pages());
    let first = messages_page(&client, &inbox(), None, None, None, &[], 200)
        .await
        .unwrap();
    let token = first.next_page.expect("a continuation");
    messages_page(&client, &inbox(), None, Some(&token), None, &[], 200)
        .await
        .unwrap();
    assert_eq!(asked(&prefers), [size(200), size(200)]);
}

#[tokio::test]
async fn a_delta_from_a_stored_cursor_asks_for_the_batch() {
    let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-inbox/delta-token-1");
    let (client, prefers) = fake_client_recording(vec![("delta-token-1", Ok(json(SNAPSHOT)))]);
    messages_page(&client, &inbox(), Some(&cursor), None, None, &[], 50)
        .await
        .unwrap();
    assert_eq!(asked(&prefers), [size(50)]);
}

#[tokio::test]
async fn no_batch_asks_for_the_most_a_page_holds_and_a_larger_one_is_capped() {
    for (batch, expected) in [(0, MAX_DELTA_PAGE), (MAX_DELTA_PAGE * 10, MAX_DELTA_PAGE)] {
        let (client, prefers) = fake_client_recording(vec![("messages/delta", Ok(json(SNAPSHOT)))]);
        messages_page(&client, &inbox(), None, None, None, &[], batch)
            .await
            .unwrap();
        assert_eq!(asked(&prefers), [size(expected)], "fetch_batch {batch}");
    }
}

#[tokio::test]
async fn the_stream_hands_its_fetch_batch_to_every_page() {
    let (client, prefers) = fake_client_recording(two_pages());
    let provider = GraphProvider::new(client, inbox());
    let account = AccountId::try_from("acct-1").unwrap();
    let mut stream = provider.stream_email(&account, None, MailboxWindows::default(), 75, 0);
    while let Some(chunk) = stream.next().await {
        chunk.unwrap();
    }
    assert_eq!(asked(&prefers), [size(75), size(75)]);
}
