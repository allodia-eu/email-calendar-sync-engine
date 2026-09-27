//! Offline tests for [`fetch_batch`], over a fake executor that records what it was sent.

use engine_core::{
    error::FailureClass,
    ids::{BlobId, MailboxId, MessageId},
    mail::Message,
    membership::Memberships,
};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{INLINE_MAX, fetch_batch};
use crate::{executor::Executor, provider::provider_test_support::FakeExecutor};

fn session() -> Value {
    json!({
        "capabilities": {
            "urn:ietf:params:jmap:core": { "maxObjectsInGet": 500, "maxConcurrentRequests": 4 },
            "urn:ietf:params:jmap:mail": {},
            "urn:ietf:params:jmap:blob": {}
        },
        "accounts": { "c": { "accountCapabilities": { "urn:ietf:params:jmap:blob": {} } } },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "apiUrl": "https://mail.test.local/jmap/",
        "downloadUrl": "https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}"
    })
}

fn message(id: &str, blob: &str, size: Option<u64>) -> Message {
    let mut message = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    );
    message.blob_id = Some(BlobId::try_from(blob).unwrap());
    message.size = size;
    message
}

fn answer(list: Value, not_found: Value) -> Value {
    let mut result = json!({ "accountId": "c" });
    result["list"] = list;
    result["notFound"] = not_found;
    json!({ "methodResponses": [["Blob/get", result, "0"]], "sessionState": "s" })
}

/// Every `(index, outcome)` the batch yields, sorted by index.
async fn run(
    executor: &FakeExecutor,
    messages: &[Message],
) -> Vec<(usize, Result<Vec<u8>, FailureClass>)> {
    let mut outcomes: Vec<_> = fetch_batch(executor, messages)
        .map(|(index, result)| {
            (
                index,
                result
                    .map(|raw| raw.as_bytes().to_vec())
                    .map_err(|err| err.class()),
            )
        })
        .collect()
        .await;
    outcomes.sort_by_key(|(index, _)| *index);
    outcomes
}

#[test]
fn only_an_account_that_carries_the_extension_is_batched() {
    // RFC 9404 advertises per account; a session that lists the extension for some other
    // account says nothing about the mail account's blobs.
    assert!(
        FakeExecutor::from_session(&session(), vec![])
            .session()
            .blob_get()
    );
    let mut elsewhere = session();
    elsewhere["accounts"] = json!({
        "c": { "accountCapabilities": {} },
        "other": { "accountCapabilities": { "urn:ietf:params:jmap:blob": {} } }
    });
    assert!(
        !FakeExecutor::from_session(&elsewhere, vec![])
            .session()
            .blob_get()
    );
    let mut absent = session();
    absent["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("urn:ietf:params:jmap:blob");
    assert!(
        !FakeExecutor::from_session(&absent, vec![])
            .session()
            .blob_get()
    );
}

#[tokio::test]
async fn small_messages_come_back_in_one_blob_get() {
    // `Zmlyc3Q=` is `first`, `c2Vjb25k` is `second`.
    let executor = FakeExecutor::from_session(
        &session(),
        vec![answer(
            json!([
                { "id": "B2", "data:asBase64": "c2Vjb25k", "size": 6 },
                { "id": "B1", "data:asBase64": "Zmlyc3Q=", "size": 5 }
            ]),
            json!([]),
        )],
    );
    let messages = [message("M1", "B1", Some(5)), message("M2", "B2", Some(6))];

    let outcomes = run(&executor, &messages).await;

    assert_eq!(
        outcomes,
        vec![(0, Ok(b"first".to_vec())), (1, Ok(b"second".to_vec()))]
    );
    let (using, method, arguments) = executor.sole_call();
    assert_eq!(method, "Blob/get");
    assert!(
        using.iter().any(|urn| urn == "urn:ietf:params:jmap:blob"),
        "{using:?}"
    );
    assert_eq!(arguments["ids"], json!(["B1", "B2"]));
    assert_eq!(arguments["properties"], json!(["data:asBase64", "size"]));
    assert!(executor.download_urls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_large_or_unmeasured_message_is_downloaded_rather_than_inlined() {
    // Base64 costs a third more bytes than the download, which a large message pays for in
    // more time than the round trip it would save.
    let executor = FakeExecutor::from_session(&session(), vec![]).with_download_body(b"bytes");
    let messages = [
        message("M1", "BIG", Some(INLINE_MAX + 1)),
        message("M2", "UNKNOWN", None),
    ];

    let outcomes = run(&executor, &messages).await;

    assert_eq!(
        outcomes,
        vec![(0, Ok(b"bytes".to_vec())), (1, Ok(b"bytes".to_vec()))]
    );
    assert_eq!(executor.request_count(), 0, "nothing was asked of Blob/get");
    assert_eq!(executor.download_urls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_missing_blob_fails_alone_and_a_truncated_one_is_downloaded() {
    let executor = FakeExecutor::from_session(
        &session(),
        vec![answer(
            json!([
                { "id": "B1", "data:asBase64": "Zmlyc3Q=", "size": 5 },
                { "id": "B3", "data:asBase64": "cGFy", "size": 900, "isTruncated": true }
            ]),
            json!(["B2"]),
        )],
    )
    .with_download_body(b"whole");
    let messages = [
        message("M1", "B1", Some(5)),
        message("M2", "B2", Some(5)),
        message("M3", "B3", Some(900)),
    ];

    let outcomes = run(&executor, &messages).await;

    assert_eq!(
        outcomes,
        vec![
            (0, Ok(b"first".to_vec())),
            (1, Err(FailureClass::Permanent)),
            (2, Ok(b"whole".to_vec())),
        ]
    );
    let downloads = executor.download_urls.lock().unwrap();
    assert_eq!(downloads.len(), 1);
    assert!(downloads[0].contains("/B3/"), "{downloads:?}");
}

#[tokio::test]
async fn a_server_without_blob_get_is_answered_by_downloads() {
    // A method-level refusal (`unknownMethod`) is not load: the messages are still fetched,
    // the way they always were.
    let refused = json!({
        "methodResponses": [["error", { "type": "unknownMethod" }, "0"]],
        "sessionState": "s"
    });
    let executor =
        FakeExecutor::from_session(&session(), vec![refused]).with_download_body(b"bytes");
    let messages = [message("M1", "B1", Some(5)), message("M2", "B2", Some(5))];

    let outcomes = run(&executor, &messages).await;

    assert_eq!(
        outcomes,
        vec![(0, Ok(b"bytes".to_vec())), (1, Ok(b"bytes".to_vec()))]
    );
    assert_eq!(executor.download_urls.lock().unwrap().len(), 2);
}
