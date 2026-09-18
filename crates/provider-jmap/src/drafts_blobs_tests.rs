//! Offline tests for reusing a stored draft's attachment blobs.
//!
//! The fake executor records every request and every upload it was handed, which is what
//! these assert on: the point of the module is that an unchanged attachment costs **no**
//! upload, and the offline fake is the only place that can be counted exactly.

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::{Draft, DraftAttachment};
use serde_json::json;

use super::resolve;
use crate::provider::provider_test_support::FakeExecutor;

/// A fake serving `responses`, whose uploads answer `blob_ids` in turn.
fn executor(responses: Vec<serde_json::Value>, blob_ids: Vec<&'static str>) -> FakeExecutor {
    FakeExecutor::new(responses).with_upload_blob_ids(blob_ids)
}

fn draft_with(attachments: Vec<DraftAttachment>) -> Draft {
    let mut draft = Draft::new(
        MessageIdHeader::new("compose-1@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    );
    draft.attachments = attachments;
    draft
}

fn note() -> DraftAttachment {
    DraftAttachment::attachment("note.txt", "text/plain", b"hello".to_vec())
}

/// An `Email/get` answering with one stored attachment matching [`note`].
fn stored_note() -> serde_json::Value {
    json!({ "methodResponses": [["Email/get", { "list": [
        { "id": "eaaaaaold", "attachments": [
            { "blobId": "blob-kept", "name": "note.txt", "type": "text/plain", "size": 5 }
        ]}
    ]}, "0"]]})
}

#[tokio::test]
async fn an_unchanged_attachment_reuses_the_stored_blob() {
    // The save that matters: the server already holds these bytes, so saving again must
    // not send them a second time.
    let executor = executor(vec![stored_note()], vec!["blob-fresh"]);
    let existing = engine_core::ids::ProviderKey::new("eaaaaaold").unwrap();

    let blobs = resolve(&executor, "u1", &draft_with(vec![note()]), Some(&existing))
        .await
        .unwrap();

    assert_eq!(blobs, vec!["blob-kept".to_owned()]);
    assert!(
        executor.uploads.lock().unwrap().is_empty(),
        "an unchanged attachment was uploaded again"
    );
}

#[tokio::test]
async fn an_attachment_of_a_different_size_is_uploaded() {
    // Same name and media type, different bytes. JMAP reports the *decoded* size, so the
    // change is visible here in a way Graph's encoded `size` could not show.
    let executor = executor(vec![stored_note()], vec!["blob-fresh"]);
    let existing = engine_core::ids::ProviderKey::new("eaaaaaold").unwrap();
    let edited = DraftAttachment::attachment("note.txt", "text/plain", b"hello again".to_vec());

    let blobs = resolve(&executor, "u1", &draft_with(vec![edited]), Some(&existing))
        .await
        .unwrap();

    assert_eq!(blobs, vec!["blob-fresh".to_owned()]);
    assert_eq!(executor.uploads.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_first_save_uploads_and_reads_nothing() {
    // No draft to carry anything over from, so there is nothing to ask the server for.
    let executor = executor(vec![], vec!["blob-fresh"]);

    let blobs = resolve(&executor, "u1", &draft_with(vec![note()]), None)
        .await
        .unwrap();

    assert_eq!(blobs, vec!["blob-fresh".to_owned()]);
    assert!(
        executor.requests.lock().unwrap().is_empty(),
        "a first save asked the server about a draft that does not exist"
    );
}

#[tokio::test]
async fn a_draft_with_no_attachments_asks_nothing() {
    let executor = executor(vec![], vec![]);
    let existing = engine_core::ids::ProviderKey::new("eaaaaaold").unwrap();

    let blobs = resolve(&executor, "u1", &draft_with(vec![]), Some(&existing))
        .await
        .unwrap();

    assert!(blobs.is_empty());
    assert!(executor.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn two_files_sharing_a_name_carry_over_by_count() {
    // One stored copy and two wanted, so exactly one upload is owed.
    let executor = executor(vec![stored_note()], vec!["blob-fresh"]);
    let existing = engine_core::ids::ProviderKey::new("eaaaaaold").unwrap();

    let blobs = resolve(
        &executor,
        "u1",
        &draft_with(vec![note(), note()]),
        Some(&existing),
    )
    .await
    .unwrap();

    assert_eq!(blobs, vec!["blob-kept".to_owned(), "blob-fresh".to_owned()]);
    assert_eq!(executor.uploads.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_stored_draft_is_read_with_the_property_that_carries_its_blobs() {
    // The fake answers whatever it is sent, so this is the only place the *request* is
    // checked: `attachments` is the property carrying `blobId`, and asking for the wrong
    // one would return nothing and silently re-upload every file instead.
    let executor = executor(vec![stored_note()], vec!["blob-fresh"]);
    let existing = engine_core::ids::ProviderKey::new("eaaaaaold").unwrap();

    resolve(&executor, "u1", &draft_with(vec![note()]), Some(&existing))
        .await
        .unwrap();

    let requests = executor.requests.lock().unwrap();
    let call = &requests[0]["methodCalls"][0];
    assert_eq!(call[0], "Email/get");
    assert_eq!(call[1]["ids"], json!(["eaaaaaold"]));
    assert_eq!(call[1]["properties"], json!(["attachments"]));
}
