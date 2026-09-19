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

use crate::{
    provider::GmailProvider,
    test_support::{fake_client_fallible, json},
};

/// The bytes Gmail actually returns, captured rather than invented: a create, the
/// update of that same draft, and the `404` a retried delete meets.
const CREATED: &str = include_str!("../tests/fixtures/mail/draft_created.json");
const UPDATED: &str = include_str!("../tests/fixtures/mail/draft_updated.json");
const NOT_FOUND: &str = include_str!("../tests/fixtures/error/draft_not_found.json");

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
    let p = GmailProvider::new(fake_client_fallible(vec![("/drafts", Ok(json(CREATED)))]));

    let key = p.put_draft(&account(), &draft(), None).await.unwrap();

    assert_eq!(key.as_str(), "r-draft-1");
}

#[tokio::test]
async fn saving_again_updates_that_same_draft_in_place() {
    // Routed to `drafts/r-101`, so it is the update rather than a second create: the id
    // is unchanged and the account never holds two copies.
    let p = GmailProvider::new(fake_client_fallible(vec![(
        "/drafts/r-draft-1",
        Ok(json(UPDATED)),
    )]));

    let existing = ProviderKey::new("r-draft-1").unwrap();
    let key = p
        .put_draft(&account(), &draft(), Some(&existing))
        .await
        .unwrap();

    // The draft id is unchanged; the *message* underneath it is a new one
    // (`message-20` → `message-21` across the captured pair), which is exactly why the
    // key this verb returns names the draft and not the message.
    assert_eq!(key.as_str(), "r-draft-1");
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
        "/drafts/r-draft-1",
        Err((404, json(NOT_FOUND))),
    )]));

    let key = ProviderKey::new("r-draft-1").unwrap();
    p.delete_draft(&account(), &key).await.unwrap();
}

#[tokio::test]
async fn a_refused_delete_that_is_not_absence_is_reported() {
    let p = GmailProvider::new(fake_client_fallible(vec![(
        "/drafts/r-draft-1",
        Err((429, json!({}))),
    )]));

    let key = ProviderKey::new("r-draft-1").unwrap();
    let err = p.delete_draft(&account(), &key).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::RateLimited);
}
