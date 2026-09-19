//! Offline tests for the attachment reconcile, over the fixture-replay fake.
//!
//! The fake matches a route by URL substring and ignores request bodies, so what these
//! pin is **which endpoints a given change reaches**: an unchanged attachment must cost
//! no upload, and a removed one must be deleted rather than taking the whole draft with
//! it. A route that is not listed is a `404`, so reaching an endpoint a test did not
//! expect fails it.

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, DraftAttachment};
use serde_json::json;

use super::reconcile;
use crate::test_support::fake_client_fallible;

fn message() -> ProviderKey {
    ProviderKey::new("AAMkAGmsg").unwrap()
}

fn draft_with(attachments: Vec<DraftAttachment>) -> Draft {
    let mut draft = Draft::new(
        engine_core::ids::MessageIdHeader::new("compose-1@test.local").unwrap(),
        engine_core::mail::EmailAddress::new("alice@test.local"),
        vec![engine_core::mail::EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    );
    draft.attachments = attachments;
    draft
}

fn note() -> DraftAttachment {
    DraftAttachment::attachment("note.txt", "text/plain", b"hello".to_vec())
}

/// A stored collection carrying `note.txt`.
fn listing_with_note() -> serde_json::Value {
    json!({ "value": [
        { "id": "att-1", "name": "note.txt", "contentType": "text/plain", "size": 191, "isInline": false }
    ]})
}

#[tokio::test]
async fn an_unchanged_attachment_is_left_alone() {
    // The save that matters most: nothing is uploaded and nothing is deleted, so keeping
    // a draft with a large file attached stays cheap however often it is saved. Only the
    // listing is routed, so an upload or a delete would 404 and fail this.
    let client = fake_client_fallible(vec![("/attachments", Ok(listing_with_note()))]);

    reconcile(&client, &message(), &draft_with(vec![note()]))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_attachment_the_draft_no_longer_carries_is_deleted() {
    let client = fake_client_fallible(vec![
        ("/attachments/att-1", Ok(json!({}))),
        ("/attachments", Ok(listing_with_note())),
    ]);

    // The draft kept its text and dropped the file: the message stays, the attachment goes.
    reconcile(&client, &message(), &draft_with(vec![]))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_new_attachment_is_uploaded_on_its_own() {
    let client = fake_client_fallible(vec![("/attachments", Ok(json!({ "value": [] })))]);

    reconcile(&client, &message(), &draft_with(vec![note()]))
        .await
        .unwrap();
}

#[tokio::test]
async fn two_files_sharing_a_name_are_matched_by_count() {
    // One stored copy and two wanted, so exactly one upload is owed. Matching by identity
    // alone would see "note.txt is present" and upload nothing.
    let client = fake_client_fallible(vec![("/attachments", Ok(listing_with_note()))]);

    reconcile(&client, &message(), &draft_with(vec![note(), note()]))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failed_listing_is_reported() {
    let client = fake_client_fallible(vec![("/attachments", Err((429, json!({}))))]);

    let err = reconcile(&client, &message(), &draft_with(vec![note()]))
        .await
        .unwrap_err();

    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::RateLimited
    );
}
