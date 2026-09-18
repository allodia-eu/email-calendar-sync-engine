//! Offline tests for `Email/set`-backed draft storage, driven over the fake executor.
//!
//! The fake answers canned bytes whatever it is sent, so these assert the **request
//! shape** as well as the parse: what the server would have received is the only thing
//! an offline run can get wrong in a way a live run would catch.

use engine_core::{error::FailureClass, ids::ProviderKey};
use engine_provider::Provider;
use serde_json::{Value, json};

use crate::provider::provider_test_support::{account, fixture, recording};

fn draft() -> engine_provider::Draft {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    engine_provider::Draft::new(
        MessageIdHeader::new("compose-1@test.local").unwrap(),
        EmailAddress::named("Alice", "alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    )
}

/// The `Email/set` arguments from the request the fake recorded.
fn set_arguments(request: &Value, index: usize) -> &Value {
    &request["methodCalls"][index][1]
}

#[tokio::test]
async fn saving_again_creates_the_new_message_and_destroys_the_old() {
    let (p, executor) = recording(vec![
        fixture("drafts_mailbox_get.json"),
        json!({
            "methodResponses": [["Email/set", {
                "created": { "draft": { "id": "eaaaaanew" } },
                "destroyed": ["eaaaaaold"]
            }, "0"]]
        }),
    ]);

    let old = ProviderKey::new("eaaaaaold").unwrap();
    let key = p.put_draft(&account(), &draft(), Some(&old)).await.unwrap();

    assert_eq!(key.as_str(), "eaaaaanew");
    let requests = executor.requests.lock().unwrap();
    let set = set_arguments(&requests[1], 0);
    // One method call carries both halves, so RFC 8620 §5.3's create-then-destroy order
    // is what removes the old copy: the server never holds neither.
    assert_eq!(set["destroy"], json!(["eaaaaaold"]));
    assert_eq!(
        set["create"]["draft"]["keywords"],
        json!({ "$draft": true, "$seen": true })
    );
    // Into Drafts, by the mailbox carrying the role rather than by name.
    assert_eq!(
        set["create"]["draft"]["mailboxIds"],
        json!({ "mb-drafts": true })
    );
}

#[tokio::test]
async fn a_first_save_destroys_nothing() {
    let (p, executor) = recording(vec![
        fixture("drafts_mailbox_get.json"),
        json!({
            "methodResponses": [["Email/set", {
                "created": { "draft": { "id": "eaaaaanew" } }
            }, "0"]]
        }),
    ]);

    p.put_draft(&account(), &draft(), None).await.unwrap();

    let requests = executor.requests.lock().unwrap();
    assert!(set_arguments(&requests[1], 0).get("destroy").is_none());
}

#[tokio::test]
async fn a_save_refused_for_quota_is_worth_retrying() {
    let (p, _executor) = recording(vec![
        fixture("drafts_mailbox_get.json"),
        json!({
            "methodResponses": [["Email/set", {
                "notCreated": { "draft": { "type": "overQuota" } }
            }, "0"]]
        }),
    ]);

    let err = p.put_draft(&account(), &draft(), None).await.unwrap_err();

    // Not permanent: a mailbox over quota now may be under it later, so the op parks
    // and comes back rather than discarding what the user wrote.
    assert_eq!(err.class(), FailureClass::RateLimited);
}

#[tokio::test]
async fn an_account_with_no_drafts_mailbox_says_so() {
    let (p, _executor) = recording(vec![json!({
        "methodResponses": [["Mailbox/get", { "list": [] }, "0"]]
    })]);

    let err = p.put_draft(&account(), &draft(), None).await.unwrap_err();

    // Permanent rather than retryable: no amount of waiting grows a Drafts mailbox,
    // and the caller has to keep the draft itself.
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn removing_a_draft_destroys_it() {
    let (p, executor) = recording(vec![json!({
        "methodResponses": [["Email/set", { "destroyed": ["eaaaaaold"] }, "0"]]
    })]);

    let key = ProviderKey::new("eaaaaaold").unwrap();
    p.delete_draft(&account(), &key).await.unwrap();

    let requests = executor.requests.lock().unwrap();
    assert_eq!(
        set_arguments(&requests[0], 0)["destroy"],
        json!(["eaaaaaold"])
    );
}

#[tokio::test]
async fn removing_a_draft_that_is_already_gone_is_done() {
    // The op is retryable, so the second attempt names a message the first destroyed.
    let (p, _executor) = recording(vec![json!({
        "methodResponses": [["Email/set", {
            "notDestroyed": { "eaaaaaold": { "type": "notFound" } }
        }, "0"]]
    })]);

    let key = ProviderKey::new("eaaaaaold").unwrap();
    p.delete_draft(&account(), &key).await.unwrap();
}

#[tokio::test]
async fn a_refused_destroy_that_is_not_absence_is_reported() {
    let (p, _executor) = recording(vec![json!({
        "methodResponses": [["Email/set", {
            "notDestroyed": { "eaaaaaold": { "type": "forbidden" } }
        }, "0"]]
    })]);

    let key = ProviderKey::new("eaaaaaold").unwrap();
    let err = p.delete_draft(&account(), &key).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Permanent);
}
