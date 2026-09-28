use std::time::Duration;

use engine_core::{
    ids::MessageIdHeader,
    mail::{EmailAddress, Keyword},
};
use engine_provider::{Draft, KeywordName};
use serde_json::{Value, json};

use super::Categories;
use crate::{
    normalize::message_from_json,
    normalize_state::state_from_json,
    test_support::{FakeRoute, RequestLog, fake_client_logged},
    transport::GraphClient,
};

const NOW: &[Duration] = &[Duration::ZERO, Duration::ZERO, Duration::ZERO];

/// The Sent Items lookup by `internetMessageId`, as a live mailbox answered it.
const SENT_COPY_LOOKUP: &str = include_str!("../tests/fixtures/mail/sent_copy_lookup.json");
/// The lookup for a message already carrying one of the names, as a live mailbox answered it.
const CATEGORY_IN_USE: &str = include_str!("../tests/fixtures/mail/category_in_use.json");

fn captured(fixture: &str) -> Value {
    serde_json::from_str(fixture).unwrap()
}

fn keyword() -> Keyword {
    Keyword::new("project-x").unwrap()
}

fn names() -> Vec<KeywordName> {
    vec![
        KeywordName::new(keyword(), "Project X")
            .unwrap()
            .also_known_as("Projekt X"),
    ]
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("graph-tag-0001@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    )
    .with_sent_copy_keyword(keyword())
}

fn found(categories: &[&str]) -> Value {
    json!({ "value": [{ "id": "AAMkSent1", "categories": categories }] })
}

fn none() -> Value {
    json!({ "value": [] })
}

fn client(in_use: Value, copy: Value, patch: FakeRoute) -> (GraphClient, RequestLog) {
    let (client, _, requests) = fake_client_logged(vec![
        ("categories/any", Ok(in_use)),
        ("internetMessageId", Ok(copy)),
        ("/messages/AAMkSent1", patch.clone()),
        ("/messages/message-1", patch),
    ]);
    (client, requests)
}

fn patched(requests: &RequestLog) -> Vec<Value> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, ..)| *method == "PATCH")
        .filter_map(|(_, _, body)| body.clone())
        .collect()
}

#[tokio::test]
async fn the_copy_is_found_by_its_message_id_and_given_the_category() {
    let (client, requests) = client(none(), captured(SENT_COPY_LOOKUP), Ok(json!({})));
    let kept = Categories::new(names())
        .tag_sent_copy(&client, &draft(), NOW)
        .await;

    assert_eq!(kept, [keyword()].into());
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|(method, url, _)| *method == "PATCH" && url.ends_with("/messages/message-1")),
        "the id the lookup answered is the one patched"
    );
    assert_eq!(patched(&requests), [json!({ "categories": ["Project X"] })]);
    let lookup = requests
        .lock()
        .unwrap()
        .iter()
        .find(|(_, url, _)| url.contains("internetMessageId"))
        .map(|(_, url, _)| url.clone())
        .unwrap();
    // The Message-ID is quoted as an OData literal, brackets included, and percent-encoded.
    assert!(
        lookup.contains("internetMessageId%20eq%20'%3Cgraph-tag-0001@test.local%3E'"),
        "{lookup}"
    );
    assert!(
        lookup.contains("/mailFolders/sentitems/messages?"),
        "{lookup}"
    );
}

#[tokio::test]
async fn a_name_another_device_already_uses_is_reused_rather_than_a_second_created() {
    let (client, requests) = client(
        found(&["Projekt X", "Blue"]),
        found(&["Blue"]),
        Ok(json!({})),
    );
    let categories = Categories::new(names());
    categories.tag_sent_copy(&client, &draft(), NOW).await;

    assert_eq!(
        patched(&requests),
        [json!({ "categories": ["Blue", "Projekt X"] })],
        "the person's own category stays and the one in use is added"
    );
    let asked = |requests: &RequestLog| {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, url, _)| url.contains("categories/any"))
            .count()
    };
    assert_eq!(asked(&requests), 1);
    categories.tag_sent_copy(&client, &draft(), NOW).await;
    assert_eq!(
        asked(&requests),
        1,
        "the name is settled after the first tag"
    );
}

#[tokio::test]
async fn a_copy_that_never_turns_up_keeps_nothing_and_patches_nothing() {
    let (client, requests) = client(none(), none(), Ok(json!({})));
    let kept = Categories::new(names())
        .tag_sent_copy(&client, &draft(), NOW)
        .await;

    assert!(kept.is_empty());
    assert!(patched(&requests).is_empty());
    let lookups = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, url, _)| url.contains("internetMessageId"))
        .count();
    assert_eq!(lookups, NOW.len(), "one lookup per wait in the schedule");
}

#[tokio::test]
async fn a_refused_patch_keeps_nothing() {
    let refused = Err((403, json!({ "error": { "code": "ErrorAccessDenied" } })));
    let (client, _) = client(none(), found(&[]), refused);
    let kept = Categories::new(names())
        .tag_sent_copy(&client, &draft(), NOW)
        .await;
    assert!(kept.is_empty());
}

#[tokio::test]
async fn a_keyword_without_a_name_costs_no_request() {
    let (client, requests) = client(none(), found(&[]), Ok(json!({})));
    let kept = Categories::default()
        .tag_sent_copy(&client, &draft(), NOW)
        .await;
    assert!(kept.is_empty());
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn a_named_category_reads_back_as_its_keyword_and_any_other_as_nothing() {
    let message = json!({
        "id": "m1",
        "parentFolderId": "f1",
        "categories": ["projekt x", "Blue"],
    });
    let read = message_from_json(&message, &names()).unwrap();
    assert!(read.keywords.contains(&keyword()));
    assert_eq!(
        read.keywords.len(),
        1,
        "Blue is the person's, not a keyword"
    );

    let state = state_from_json(&message, &names()).unwrap();
    assert!(state.keywords.contains(&keyword()));

    let unnamed = message_from_json(&message, &[]).unwrap();
    assert!(unnamed.keywords.is_empty());
}

#[tokio::test]
async fn the_captured_name_in_use_is_the_one_used() {
    let names = vec![
        KeywordName::new(keyword(), "Fixture categorie")
            .unwrap()
            .also_known_as("Fixture category"),
    ];
    let (client, requests) = client(captured(CATEGORY_IN_USE), found(&[]), Ok(json!({})));
    Categories::new(names)
        .tag_sent_copy(&client, &draft(), NOW)
        .await;
    assert_eq!(
        patched(&requests),
        [json!({ "categories": ["Fixture category"] })]
    );
}
