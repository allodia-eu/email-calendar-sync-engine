use engine_core::{
    error::FailureClass,
    ids::ProviderKey,
    mail::{Keyword, SystemKeyword},
};
use engine_provider::{KeywordName, MailEdit};
use serde_json::{Value, json};

use super::Categories;
use crate::{
    mutate::edit_mail,
    normalize::message_from_json,
    normalize_state::state_from_json,
    test_support::{FakeRoute, RequestLog, fake_client_logged},
    transport::GraphClient,
};

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

/// The key the send left on the copy, as `crate::submit` mints it.
fn placeholder() -> ProviderKey {
    ProviderKey::new("sent:graph-tag-0001@test.local").unwrap()
}

fn tag(target: ProviderKey) -> MailEdit {
    MailEdit::SetKeywords {
        target,
        add: [keyword()].into(),
        remove: std::collections::BTreeSet::default(),
    }
}

fn found(categories: &[&str]) -> Value {
    json!({ "value": [{ "id": "AAMkSent1", "categories": categories }] })
}

fn none() -> Value {
    json!({ "value": [] })
}

/// A mailbox answering the name-in-use query with `in_use`, the Sent Items lookup with `copy`,
/// a message read with `held` and every `PATCH` with `patch`.
fn client(in_use: Value, copy: Value, held: Value, patch: FakeRoute) -> (GraphClient, RequestLog) {
    let (client, _, requests) = fake_client_logged(vec![
        ("categories/any", Ok(in_use)),
        ("internetMessageId", Ok(copy)),
        ("$select=id,categories", Ok(held)),
        ("/messages/AAMkSent1", patch.clone()),
        ("/messages/message-1", patch),
    ]);
    (client, requests)
}

fn patched(requests: &RequestLog) -> Vec<(String, Value)> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, ..)| *method == "PATCH")
        .filter_map(|(_, url, body)| Some((url.clone(), body.clone()?)))
        .collect()
}

#[tokio::test]
async fn a_sent_copy_is_found_by_its_message_id_and_given_the_category() {
    let (client, requests) = client(none(), captured(SENT_COPY_LOOKUP), none(), Ok(json!({})));
    let receipt = edit_mail(&client, &Categories::new(names()), &tag(placeholder()))
        .await
        .unwrap();

    assert_eq!(
        receipt.message_key.as_str(),
        "message-1",
        "the op resolves to the id the lookup answered, not the placeholder"
    );
    let patches = patched(&requests);
    assert_eq!(patches.len(), 1);
    assert!(
        patches[0].0.ends_with("/messages/message-1"),
        "{}",
        patches[0].0
    );
    assert_eq!(patches[0].1, json!({ "categories": ["Project X"] }));
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

/// The send did not wait for the copy, so the edit is what meets a copy not filed yet: it is
/// retryable, and the outbox tries again on its own schedule.
#[tokio::test]
async fn a_sent_copy_not_there_yet_is_retryable_and_patches_nothing() {
    let (client, requests) = client(none(), none(), none(), Ok(json!({})));
    let err = edit_mail(&client, &Categories::new(names()), &tag(placeholder()))
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::Retryable);
    assert!(patched(&requests).is_empty());
}

#[tokio::test]
async fn a_name_another_device_already_uses_is_reused_rather_than_a_second_created() {
    let (client, requests) = client(
        found(&["Projekt X", "Blue"]),
        found(&["Blue"]),
        none(),
        Ok(json!({})),
    );
    let categories = Categories::new(names());
    edit_mail(&client, &categories, &tag(placeholder()))
        .await
        .unwrap();

    assert_eq!(
        patched(&requests)[0].1,
        json!({ "categories": ["Blue", "Projekt X"] }),
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
    edit_mail(&client, &categories, &tag(placeholder()))
        .await
        .unwrap();
    assert_eq!(
        asked(&requests),
        1,
        "the name is settled after the first tag"
    );
}

/// Any synced message can take a named keyword, together with a system one in the same
/// `PATCH`, and keeps the categories it already had.
#[tokio::test]
async fn a_synced_message_takes_the_category_beside_its_own_and_a_system_keyword() {
    let (client, requests) = client(
        none(),
        none(),
        json!({ "id": "AAMkSent1", "categories": ["Blue"] }),
        Ok(json!({})),
    );
    let edit = MailEdit::SetKeywords {
        target: ProviderKey::new("AAMkSent1").unwrap(),
        add: [keyword(), Keyword::system(SystemKeyword::Seen)].into(),
        remove: std::collections::BTreeSet::default(),
    };
    let receipt = edit_mail(&client, &Categories::new(names()), &edit)
        .await
        .unwrap();

    assert_eq!(receipt.message_key.as_str(), "AAMkSent1");
    let patches = patched(&requests);
    assert_eq!(patches.len(), 1, "one PATCH carries both halves");
    assert_eq!(
        patches[0].1,
        json!({ "isRead": true, "categories": ["Blue", "Project X"] })
    );
}

/// Clearing a named keyword takes every name it goes by, in any case, and nothing else.
#[tokio::test]
async fn clearing_a_named_keyword_takes_each_of_its_names_off() {
    let (client, requests) = client(
        none(),
        none(),
        json!({ "id": "AAMkSent1", "categories": ["projekt x", "Blue", "Project X"] }),
        Ok(json!({})),
    );
    let edit = MailEdit::SetKeywords {
        target: ProviderKey::new("AAMkSent1").unwrap(),
        add: std::collections::BTreeSet::default(),
        remove: [keyword()].into(),
    };
    edit_mail(&client, &Categories::new(names()), &edit)
        .await
        .unwrap();
    assert_eq!(patched(&requests)[0].1, json!({ "categories": ["Blue"] }));
}

#[tokio::test]
async fn a_refused_patch_is_the_edit_s_failure() {
    let refused = Err((403, json!({ "error": { "code": "ErrorAccessDenied" } })));
    let (client, _) = client(none(), found(&[]), none(), refused);
    let err = edit_mail(&client, &Categories::new(names()), &tag(placeholder()))
        .await
        .unwrap_err();
    assert_ne!(err.class(), FailureClass::Retryable);
}

/// A keyword the host gave no name is refused before any request, as it always was: Graph has
/// nowhere to keep it, and a silent no-op would read as done.
#[tokio::test]
async fn a_keyword_without_a_name_is_refused_and_costs_no_request() {
    let (client, requests) = client(none(), found(&[]), none(), Ok(json!({})));
    let err = edit_mail(&client, &Categories::default(), &tag(placeholder()))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
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
    let (client, requests) = client(captured(CATEGORY_IN_USE), found(&[]), none(), Ok(json!({})));
    edit_mail(&client, &Categories::new(names), &tag(placeholder()))
        .await
        .unwrap();
    assert_eq!(
        patched(&requests)[0].1,
        json!({ "categories": ["Fixture category"] })
    );
}
