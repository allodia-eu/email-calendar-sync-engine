//! Offline tests for Graph-backed draft storage, over the fixture-replay fake.
//!
//! The fake serves a matched route's canned answer regardless of the body sent, so these
//! assert the routing and the outcome; that the bytes are valid base64 MIME is the
//! mock-server transport test's job and the live test's.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::{Draft, Provider};
use serde_json::json;

use crate::{
    provider::GraphProvider,
    test_support::{fake_client_fallible, json},
};

/// The two answers a real mailbox gives for a message that is not there, captured
/// from it rather than invented: retrying `permanentDelete` on a purged draft (a
/// `404`), and the `403` the soft-delete endpoint gives for a message it already
/// moved, which `mutate.rs` also records from the purge path during retention.
const NOT_FOUND: &str = include_str!("../tests/fixtures/error/draft_delete_not_found.json");
const ALREADY_DELETED: &str =
    include_str!("../tests/fixtures/error/draft_delete_already_deleted.json");

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn provider_over(client: crate::transport::GraphClient) -> GraphProvider {
    GraphProvider::new(client, MailboxId::try_from("folder-inbox").unwrap())
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
async fn a_first_save_creates_the_message_and_returns_its_id() {
    let p = provider_over(fake_client_fallible(vec![(
        "/messages",
        Ok(json!({ "id": "AAMkAGnew" })),
    )]));

    let key = p.put_draft(&account(), &draft(), None).await.unwrap();

    assert_eq!(key.as_str(), "AAMkAGnew");
}

#[tokio::test]
async fn a_replacement_that_cannot_purge_the_old_copy_is_still_saved() {
    // The replacement path, reached here by an iTIP part, which no message-resource
    // property expresses. The create landed and the purge was refused: failing would have
    // the caller retry the whole save and store a third copy, so a duplicate draft is the
    // better loss.
    let p = provider_over(fake_client_fallible(vec![
        (
            "/messages/AAMkAGold/permanentDelete",
            Err((403, json!({ "error": { "code": "ErrorAccessDenied" } }))),
        ),
        ("/messages", Ok(json!({ "id": "AAMkAGnew" }))),
    ]));

    let mut invitation = draft();
    invitation.calendar = Some(engine_provider::DraftCalendar::new(
        engine_core::scheduling::ScheduleMethod::Request,
        "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
    ));

    let old = engine_core::ids::ProviderKey::new("AAMkAGold").unwrap();
    let key = p
        .put_draft(&account(), &invitation, Some(&old))
        .await
        .unwrap();

    assert_eq!(key.as_str(), "AAMkAGnew");
}

#[tokio::test]
async fn a_create_that_returned_no_id_is_a_protocol_error() {
    let p = provider_over(fake_client_fallible(vec![(
        "/messages",
        Ok(json!({ "subject": "Hi" })),
    )]));

    let err = p.put_draft(&account(), &draft(), None).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn removing_a_draft_that_is_already_purged_is_done() {
    // The shape a retry meets on this endpoint, observed live: the op is retryable, so
    // the second attempt names a draft the first one purged.
    let p = provider_over(fake_client_fallible(vec![(
        "/messages/AAMkAGold/permanentDelete",
        Err((404, json(NOT_FOUND))),
    )]));

    let key = engine_core::ids::ProviderKey::new("AAMkAGold").unwrap();
    p.delete_draft(&account(), &key).await.unwrap();
}

#[tokio::test]
async fn removing_a_draft_that_graph_refuses_to_delete_again_is_done() {
    // The other way Graph says gone, which no spec reading predicts: a `403` whose code
    // is `ErrorCannotDeleteObject`. Observed on the soft-delete endpoint and recorded
    // by `mutate.rs` from the purge path during retention, so it is accepted here too
    // rather than parking a retryable op on a message that is not there.
    let p = provider_over(fake_client_fallible(vec![(
        "/messages/AAMkAGold/permanentDelete",
        Err((403, json(ALREADY_DELETED))),
    )]));

    let key = engine_core::ids::ProviderKey::new("AAMkAGold").unwrap();
    p.delete_draft(&account(), &key).await.unwrap();
}

#[tokio::test]
async fn a_delete_refused_for_permission_still_fails() {
    // The guard is the error *code*, not the `403`: a mailbox the token may not write
    // answers with the same status and must not be mistaken for an absent message.
    let p = provider_over(fake_client_fallible(vec![(
        "/messages/AAMkAGold/permanentDelete",
        Err((403, json!({ "error": { "code": "ErrorAccessDenied" } }))),
    )]));

    let key = engine_core::ids::ProviderKey::new("AAMkAGold").unwrap();
    let err = p.delete_draft(&account(), &key).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn a_refused_delete_that_is_not_absence_is_reported() {
    let p = provider_over(fake_client_fallible(vec![(
        "/messages/AAMkAGold/permanentDelete",
        Err((429, json!({}))),
    )]));

    let key = engine_core::ids::ProviderKey::new("AAMkAGold").unwrap();
    let err = p.delete_draft(&account(), &key).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::RateLimited);
}
