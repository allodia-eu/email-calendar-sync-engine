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
async fn adding_an_attachment_keeps_the_draft_and_uploads_only_the_file() {
    // An attachment is a resource of its own, so gaining one costs an upload, not a new
    // message. Neither the create nor the purge is routed, so reaching either fails this.
    let p = provider_over(fake_client_fallible(vec![
        (
            "/messages/AAMkAGold/attachments",
            Ok(json!({ "value": [] })),
        ),
        ("/messages/AAMkAGold", Ok(fixture(PATCHED))),
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

    assert_eq!(
        key,
        existing(),
        "gaining an attachment keeps the draft's id"
    );
}

#[tokio::test]
async fn removing_an_attachment_deletes_it_without_touching_the_draft() {
    // The `PATCH` response says the stored message still has attachments the draft no
    // longer carries, so the collection is reconciled. The message itself is untouched:
    // no create and no purge are routed.
    let p = provider_over(fake_client_fallible(vec![
        ("/messages/AAMkAGold/attachments/att-1", Ok(json!({}))),
        (
            "/messages/AAMkAGold/attachments",
            Ok(json!({ "value": [
                { "id": "att-1", "name": "note.txt", "contentType": "text/plain", "isInline": false }
            ]})),
        ),
        (
            "/messages/AAMkAGold",
            Ok(json!({ "id": "AAMkAGold", "hasAttachments": true })),
        ),
    ]));

    let key = p
        .put_draft(&account(), &draft(), Some(&existing()))
        .await
        .unwrap();

    assert_eq!(key, existing(), "losing an attachment keeps the draft's id");
}

#[tokio::test]
async fn a_draft_carrying_an_itip_part_is_replaced() {
    // The one shape no message-resource property expresses: a body part with `method=`,
    // which only the MIME assembler can write. So this save does store a replacement,
    // and the key moves.
    let p = provider_over(fake_client_fallible(vec![
        ("/messages/AAMkAGold/permanentDelete", Ok(json!({}))),
        ("/messages", Ok(json!({ "id": "AAMkAGnew" }))),
    ]));

    let mut invitation = draft();
    invitation.calendar = Some(engine_provider::DraftCalendar::new(
        engine_core::scheduling::ScheduleMethod::Request,
        "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
    ));

    let key = p
        .put_draft(&account(), &invitation, Some(&existing()))
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
