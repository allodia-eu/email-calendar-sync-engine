//! Offline tests for the in-place draft rewrite, over the fixture-replay fake.
//!
//! The fake matches a route by URL substring and ignores the body, so what these pin is
//! **which endpoint a given save reaches** — the whole point of this module is that a
//! text edit reaches `PATCH` and an attachment change does not.

use engine_core::{
    ids::{MessageIdHeader, ProviderKey},
    mail::EmailAddress,
};
use engine_provider::{Draft, DraftAttachment, Provider};
use serde_json::json;

use crate::{
    provider::GraphProvider,
    test_support::{fake_client_fallible, json as fixture},
};

const PATCHED: &str = include_str!("../tests/fixtures/mail/draft_patched.json");

fn account() -> engine_core::ids::AccountId {
    engine_core::ids::AccountId::try_from("acct-1").unwrap()
}

fn provider_over(client: crate::transport::GraphClient) -> GraphProvider {
    GraphProvider::new(
        client,
        engine_core::ids::MailboxId::try_from("folder-inbox").unwrap(),
    )
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("compose-1@test.local").unwrap(),
        EmailAddress::named("Alice", "alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    )
}

fn existing() -> ProviderKey {
    ProviderKey::new("AAMkAGold").unwrap()
}

#[tokio::test]
async fn a_text_edit_rewrites_the_stored_draft_and_keeps_its_key() {
    // The autosave shape: one request, and the key the caller already had. Only the
    // PATCH route is served, so reaching the create would fail the test outright.
    let p = provider_over(fake_client_fallible(vec![(
        "/messages/AAMkAGold",
        Ok(fixture(PATCHED)),
    )]));

    let key = p
        .put_draft(&account(), &draft(), Some(&existing()))
        .await
        .unwrap();

    assert_eq!(key, existing(), "a rewritten draft keeps its id");
}

#[tokio::test]
async fn adding_an_attachment_stores_a_replacement_instead() {
    // `PATCH` cannot touch the attachment collection, so this must not reach it: only
    // the create and the purge are routed.
    let p = provider_over(fake_client_fallible(vec![
        ("/messages/AAMkAGold/permanentDelete", Ok(json!({}))),
        ("/messages", Ok(json!({ "id": "AAMkAGnew" }))),
    ]));

    let mut with_file = draft();
    with_file.attachments = vec![DraftAttachment::attachment(
        "note.txt",
        "text/plain",
        b"hello".to_vec(),
    )];

    let key = p
        .put_draft(&account(), &with_file, Some(&existing()))
        .await
        .unwrap();

    assert_eq!(key.as_str(), "AAMkAGnew", "the key moves on a replacement");
}

#[tokio::test]
async fn a_draft_that_still_holds_attachments_is_replaced_not_rewritten() {
    // The remove-an-attachment case. The `PATCH` lands but the response says the stored
    // message kept its attachments, which only a replacement can undo.
    let p = provider_over(fake_client_fallible(vec![
        ("/messages/AAMkAGold/permanentDelete", Ok(json!({}))),
        (
            "/messages/AAMkAGold",
            Ok(json!({ "id": "AAMkAGold", "hasAttachments": true })),
        ),
        ("/messages", Ok(json!({ "id": "AAMkAGnew" }))),
    ]));

    let key = p
        .put_draft(&account(), &draft(), Some(&existing()))
        .await
        .unwrap();

    assert_eq!(key.as_str(), "AAMkAGnew");
}

#[tokio::test]
async fn a_draft_deleted_elsewhere_is_stored_fresh() {
    // Deleted from another device between saves. Failing here would lose what the user
    // has open; storing a new copy is the recoverable answer.
    let p = provider_over(fake_client_fallible(vec![
        (
            "/messages/AAMkAGold/permanentDelete",
            Err((404, json!({ "error": { "code": "ErrorItemNotFound" } }))),
        ),
        (
            "/messages/AAMkAGold",
            Err((404, json!({ "error": { "code": "ErrorItemNotFound" } }))),
        ),
        ("/messages", Ok(json!({ "id": "AAMkAGnew" }))),
    ]));

    let key = p
        .put_draft(&account(), &draft(), Some(&existing()))
        .await
        .unwrap();

    assert_eq!(key.as_str(), "AAMkAGnew");
}

#[tokio::test]
async fn a_rewrite_refused_for_rate_limiting_does_not_become_a_second_draft() {
    // The failure that must *not* fall through: a throttled PATCH is retryable, and
    // creating a replacement instead would leave the account holding two drafts.
    let p = provider_over(fake_client_fallible(vec![
        ("/messages/AAMkAGold", Err((429, json!({})))),
        ("/messages", Ok(json!({ "id": "AAMkAGnew" }))),
    ]));

    let err = p
        .put_draft(&account(), &draft(), Some(&existing()))
        .await
        .unwrap_err();

    assert_eq!(err.class(), engine_core::error::FailureClass::RateLimited);
}
