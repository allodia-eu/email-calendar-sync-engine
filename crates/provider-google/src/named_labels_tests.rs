use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, MailState, Message},
    membership::Memberships,
};
use engine_provider::{Draft, KeywordName};
use serde_json::{Value, json};

use super::{Labels, NamedLabels};
use crate::{
    fetch,
    test_support::{FakeRoute, Request, recording_client},
    transport::GoogleClient,
};

/// The captured label list: system labels, and the user labels `Label_1`…`Label_4`.
const LABELS: &str = include_str!("../tests/fixtures/mail/labels.json");
/// A live `labels.create` answer for a hidden label (`Label_6`).
const CREATED: &str = include_str!("../tests/fixtures/mail/label_created_hidden.json");
/// A live `409` for a create whose name the account already has.
const CONFLICT: &str = include_str!("../tests/fixtures/error/label_name_conflict.json");

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn keyword() -> Keyword {
    Keyword::new("project-x").unwrap()
}

/// `Fixture Label` (the captured `Label_1`) is one of the keyword's names.
fn names() -> Vec<KeywordName> {
    vec![
        KeywordName::new(keyword(), "Project X")
            .unwrap()
            .also_known_as("fixture label"),
    ]
}

fn unnamed_list() -> Value {
    json!({ "labels": [{ "id": "INBOX", "name": "INBOX", "type": "system" }] })
}

fn captured_list() -> Value {
    serde_json::from_str(LABELS).unwrap()
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("gmail-tag-0001@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    )
    .with_sent_copy_keyword(keyword())
}

fn sent() -> ProviderKey {
    ProviderKey::new("sent-message-1").unwrap()
}

type Log = std::sync::Arc<std::sync::Mutex<Vec<Request>>>;

fn routed(list: Value, create: FakeRoute, modify: FakeRoute) -> (GoogleClient, Log) {
    recording_client(vec![
        ("/modify", modify),
        ("POST /labels", create),
        ("GET /labels", Ok(list)),
    ])
}

fn posted(log: &Log, path: &str) -> Vec<Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|request| request.method == "POST" && request.url.ends_with(path))
        .filter_map(|request| request.body.clone())
        .collect()
}

#[test]
fn a_captured_label_under_any_name_stands_for_the_keyword() {
    let list = captured_list();
    let named = NamedLabels::from_list(list["labels"].as_array().unwrap(), &names());
    assert!(named.contains("Label_1"), "matched ignoring case");
    assert!(
        !named.contains("Label_2"),
        "a nested label has a name of its own"
    );
    assert!(!named.contains("INBOX"));
}

#[test]
fn a_named_label_moves_from_the_memberships_onto_the_keywords() {
    let named = NamedLabels::from_list(captured_list()["labels"].as_array().unwrap(), &names());
    let mut message = Message::new(
        MessageId::try_from("m1").unwrap(),
        Memberships::new(["SENT", "Label_1"].map(|id| MailboxId::try_from(id).unwrap())).unwrap(),
    );
    named.apply(&mut message);
    assert!(message.keywords.contains(&keyword()));
    assert_eq!(
        message.mailboxes,
        Memberships::of_one(MailboxId::try_from("SENT").unwrap())
    );

    let mut alone = Message::new(
        MessageId::try_from("m2").unwrap(),
        Memberships::of_one(MailboxId::try_from("Label_1").unwrap()),
    );
    named.apply(&mut alone);
    assert_eq!(
        alone.mailboxes,
        Memberships::of_one(MailboxId::try_from("ALL_MAIL").unwrap()),
        "a message filed nowhere else falls back to All Mail, as the normalizer does"
    );

    let mut state = MailState::default().filed_in(
        Memberships::new(["INBOX", "Label_1"].map(|id| MailboxId::try_from(id).unwrap())).unwrap(),
    );
    named.apply_state(&mut state);
    assert!(state.keywords.contains(&keyword()));
    assert_eq!(
        state.mailboxes,
        Some(Memberships::of_one(MailboxId::try_from("INBOX").unwrap()))
    );
}

#[tokio::test]
async fn a_named_label_is_not_a_folder() {
    let (client, _) = routed(captured_list(), Ok(json!({})), Ok(json!({})));
    let (mailboxes, named) = fetch::labels(&client, &names()).await.unwrap();
    assert!(named.contains("Label_1"));
    assert!(
        mailboxes
            .iter()
            .all(|mailbox| mailbox.id.as_str() != "Label_1")
    );
    let (all, _) = fetch::labels(&client, &[]).await.unwrap();
    assert!(all.iter().any(|mailbox| mailbox.id.as_str() == "Label_1"));
}

#[tokio::test]
async fn a_label_the_account_has_under_any_name_is_used_as_it_is() {
    let (client, log) = routed(captured_list(), Ok(fixture(CREATED)), Ok(json!({})));
    let kept = Labels::new(names())
        .tag_sent_copy(&client, &sent(), &draft())
        .await;
    assert_eq!(kept, [keyword()].into());
    assert!(posted(&log, "/labels").is_empty(), "nothing created");
    assert_eq!(
        posted(&log, "/messages/sent-message-1/modify"),
        [json!({ "addLabelIds": ["Label_1"], "removeLabelIds": [] })]
    );
}

#[tokio::test]
async fn with_no_label_under_any_name_one_is_created_hidden_from_the_label_list() {
    let (client, log) = routed(unnamed_list(), Ok(fixture(CREATED)), Ok(json!({})));
    let labels = Labels::new(names());
    let kept = labels.tag_sent_copy(&client, &sent(), &draft()).await;
    assert_eq!(kept, [keyword()].into());
    assert_eq!(
        posted(&log, "/labels"),
        [json!({
            "name": "Project X",
            "labelListVisibility": "labelHide",
            "messageListVisibility": "show",
        })]
    );
    assert_eq!(
        posted(&log, "/messages/sent-message-1/modify"),
        [json!({ "addLabelIds": ["Label_6"], "removeLabelIds": [] })]
    );
    let named = labels.resolved(&client).await.unwrap();
    assert!(
        named.contains("Label_6"),
        "the new label is read as the keyword before the next label-list read"
    );
}

#[tokio::test]
async fn a_label_that_cannot_be_had_or_a_refused_modify_keeps_nothing() {
    let conflict = Err((409, fixture(CONFLICT)));
    let (client, log) = routed(unnamed_list(), conflict, Ok(json!({})));
    let kept = Labels::new(names())
        .tag_sent_copy(&client, &sent(), &draft())
        .await;
    assert!(kept.is_empty());
    assert!(posted(&log, "/modify").is_empty());

    let refused = Err((403, json!({ "error": { "code": 403 } })));
    let (refusing, _) = routed(captured_list(), Ok(json!({})), refused);
    let kept = Labels::new(names())
        .tag_sent_copy(&refusing, &sent(), &draft())
        .await;
    assert!(kept.is_empty());
}

#[tokio::test]
async fn a_keyword_without_a_name_costs_no_request() {
    let (client, log) = routed(captured_list(), Ok(json!({})), Ok(json!({})));
    let kept = Labels::default()
        .tag_sent_copy(&client, &sent(), &draft())
        .await;
    assert!(kept.is_empty());
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(
        Labels::default().resolved(&client).await.unwrap(),
        NamedLabels::default()
    );
}

#[test]
fn a_hidden_label_as_gmail_lists_it_stands_for_the_keyword() {
    let registered = [KeywordName::new(keyword(), "Fixture keyword label").unwrap()];
    let listed = NamedLabels::from_list(&[fixture(CREATED)], &registered);
    assert!(listed.contains("Label_6"));
}
