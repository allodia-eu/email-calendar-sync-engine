//! Offline tests for Gmail-backed draft storage, over the fixture-replay fake.
//!
//! The fake serves a matched route's canned answer regardless of the body sent, so what
//! these pin is the **route**: which of `drafts.create` / `drafts.update` /
//! `drafts.delete` a given call reaches, which is the whole difference between Gmail's
//! draft model and every other adapter's here.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, Message, MessageMetadata},
    membership::Memberships,
};
use engine_provider::{Draft, KeywordName, Provider};
use serde_json::{Value, json};

use crate::{
    named_labels::NamedLabels,
    provider::GmailProvider,
    test_support::{FakeRoute, Request, fake_client_fallible, json, recording_client},
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

    let key = p
        .put_draft(&account(), &draft(), None, &MessageMetadata::default())
        .await
        .unwrap()
        .key;

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
        .put_draft(
            &account(),
            &draft(),
            Some(&existing),
            &MessageMetadata::default(),
        )
        .await
        .unwrap()
        .key;

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

    let err = p
        .put_draft(&account(), &draft(), None, &MessageMetadata::default())
        .await
        .unwrap_err();

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

/// The captured label list, whose `Label_1` (`Fixture Label`) is one of the keyword's names.
const LABELS: &str = include_str!("../tests/fixtures/mail/labels.json");

fn keyword() -> Keyword {
    Keyword::new("project-x").unwrap()
}

fn tagged() -> BTreeSet<Keyword> {
    BTreeSet::from([keyword()])
}

fn named(p: GmailProvider) -> GmailProvider {
    p.with_keyword_names(vec![
        KeywordName::new(keyword(), "Project X")
            .unwrap()
            .also_known_as("fixture label"),
    ])
}

type Log = std::sync::Arc<std::sync::Mutex<Vec<Request>>>;

fn labelling(stored: (&str, &str), modify: FakeRoute) -> (GmailProvider, Log) {
    let (client, log) = recording_client(vec![
        ("/modify", modify),
        ("GET /labels", Ok(json(LABELS))),
        (stored.0, Ok(json(stored.1))),
    ]);
    (named(GmailProvider::new(client)), log)
}

/// The `addLabelIds` of each `messages.modify` the log holds, by the message it names.
fn labelled(log: &Log) -> Vec<(String, Value)> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|request| request.url.ends_with("/modify"))
        .map(|request| {
            let body = request.body.clone().unwrap_or_default();
            (request.url.clone(), body["addLabelIds"].clone())
        })
        .collect()
}

#[tokio::test]
async fn a_save_labels_the_message_under_the_draft_and_reports_the_keyword_kept() {
    let (p, log) = labelling(("/drafts", CREATED), Ok(json!({})));

    let receipt = p
        .put_draft(
            &account(),
            &draft(),
            None,
            &MessageMetadata::default().with_keywords(tagged()),
        )
        .await
        .unwrap();

    assert_eq!(receipt.key.as_str(), "r-draft-1");
    assert_eq!(receipt.kept.keywords, tagged());
    let labelled = labelled(&log);
    assert_eq!(labelled.len(), 1);
    assert!(
        labelled[0].0.ends_with("/messages/message-20/modify"),
        "{labelled:?}"
    );
    assert_eq!(labelled[0].1, json!(["Label_1"]));
}

#[tokio::test]
async fn every_save_labels_the_new_message_again() {
    // `drafts.update` replaces the message and its labels with it, so the re-save labels
    // the message it now holds rather than trusting the last one's.
    let (p, log) = labelling(("/drafts/r-draft-1", UPDATED), Ok(json!({})));
    let existing = ProviderKey::new("r-draft-1").unwrap();

    let receipt = p
        .put_draft(
            &account(),
            &draft(),
            Some(&existing),
            &MessageMetadata::default().with_keywords(tagged()),
        )
        .await
        .unwrap();

    assert_eq!(receipt.kept.keywords, tagged());
    assert!(labelled(&log)[0].0.ends_with("/messages/message-21/modify"));
}

#[tokio::test]
async fn a_save_asking_for_no_keyword_costs_no_label_request() {
    let (p, log) = labelling(("/drafts", CREATED), Ok(json!({})));

    let receipt = p
        .put_draft(&account(), &draft(), None, &MessageMetadata::default())
        .await
        .unwrap();

    assert_eq!(receipt.kept.keywords, BTreeSet::new());
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "drafts.create and nothing else"
    );
}

#[tokio::test]
async fn a_refused_label_leaves_the_draft_saved_without_the_keyword() {
    let (p, _log) = labelling(("/drafts", CREATED), Err((400, json!({}))));

    let receipt = p
        .put_draft(
            &account(),
            &draft(),
            None,
            &MessageMetadata::default().with_keywords(tagged()),
        )
        .await
        .unwrap();

    assert_eq!(receipt.key.as_str(), "r-draft-1");
    assert_eq!(receipt.kept.keywords, BTreeSet::new());
}

#[tokio::test]
async fn a_keyword_without_a_name_is_not_kept_and_costs_no_label_request() {
    let (client, log) = recording_client(vec![("/drafts", Ok(json(CREATED)))]);
    let p = GmailProvider::new(client);
    assert!(!p.connection_info().capabilities.draft_keywords());

    let receipt = p
        .put_draft(
            &account(),
            &draft(),
            None,
            &MessageMetadata::default().with_keywords(tagged()),
        )
        .await
        .unwrap();

    assert_eq!(receipt.kept.keywords, BTreeSet::new());
    assert_eq!(log.lock().unwrap().len(), 1);
    assert!(named(p).connection_info().capabilities.draft_keywords());
}

/// A synced draft carrying the label reads back as a draft carrying the keyword, so a host
/// resuming it on another device sees it.
#[test]
fn a_synced_draft_reads_its_label_back_as_the_keyword() {
    let list = json(LABELS);
    let names = vec![KeywordName::new(keyword(), "fixture label").unwrap()];
    let labels = NamedLabels::from_list(list["labels"].as_array().unwrap(), &names);
    let mut message = Message::new(
        MessageId::try_from("message-20").unwrap(),
        Memberships::new(["DRAFT", "Label_1"].map(|id| MailboxId::try_from(id).unwrap())).unwrap(),
    );

    labels.apply(&mut message);

    assert!(message.keywords.contains(&keyword()));
    assert_eq!(
        message.mailboxes,
        Memberships::of_one(MailboxId::try_from("DRAFT").unwrap())
    );
}
