//! Offline tests for Gmail-backed draft storage, over the fixture-replay fake.
//!
//! The fake serves a matched route's canned answer regardless of the body sent, so what
//! these pin is the **route**: which of `drafts.create` / `drafts.update` /
//! `drafts.delete` a given call reaches, which is the whole difference between Gmail's
//! draft model and every other adapter's here.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MessageIdHeader, ProviderKey},
    mail::EmailAddress,
};
use engine_provider::{Draft, Provider};
use serde_json::json;

use crate::{provider::GmailProvider, test_support::fake_client_fallible};

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
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

#[tokio::test]
async fn a_first_save_creates_a_draft_and_returns_its_draft_id() {
    // The key is the draft's own id, not the message's: Gmail's draft outlives each
    // message it wraps.
    let p = GmailProvider::new(fake_client_fallible(vec![(
        "/drafts",
        Ok(json!({ "id": "r-101", "message": { "id": "18cmsg", "threadId": "18cthr" } })),
    )]));

    let key = p.put_draft(&account(), &draft(), None).await.unwrap();

    assert_eq!(key.as_str(), "r-101");
}

#[tokio::test]
async fn saving_again_updates_that_same_draft_in_place() {
    // Routed to `drafts/r-101`, so it is the update rather than a second create: the id
    // is unchanged and the account never holds two copies.
    let p = GmailProvider::new(fake_client_fallible(vec![(
        "/drafts/r-101",
        Ok(json!({ "id": "r-101", "message": { "id": "18cnewer" } })),
    )]));

    let existing = ProviderKey::new("r-101").unwrap();
    let key = p
        .put_draft(&account(), &draft(), Some(&existing))
        .await
        .unwrap();

    assert_eq!(key.as_str(), "r-101");
}

#[tokio::test]
async fn a_store_that_returned_no_id_is_a_protocol_error() {
    let p = GmailProvider::new(fake_client_fallible(vec![(
        "/drafts",
        Ok(json!({ "message": { "id": "18cmsg" } })),
    )]));

    let err = p.put_draft(&account(), &draft(), None).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn removing_a_draft_that_is_already_gone_is_done() {
    // The op is retryable, so the second attempt names a draft the first deleted.
    let p = GmailProvider::new(fake_client_fallible(vec![(
        "/drafts/r-101",
        Err((404, json!({ "error": { "code": 404 } }))),
    )]));

    let key = ProviderKey::new("r-101").unwrap();
    p.delete_draft(&account(), &key).await.unwrap();
}

#[tokio::test]
async fn a_refused_delete_that_is_not_absence_is_reported() {
    let p = GmailProvider::new(fake_client_fallible(vec![(
        "/drafts/r-101",
        Err((429, json!({}))),
    )]));

    let key = ProviderKey::new("r-101").unwrap();
    let err = p.delete_draft(&account(), &key).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::RateLimited);
}
