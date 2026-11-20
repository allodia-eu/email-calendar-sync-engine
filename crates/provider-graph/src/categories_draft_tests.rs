//! A stored draft's keywords as categories, through `put_draft`: set after a create, taken off
//! by a re-save in place that no longer asks for them, and never at the cost of the save.

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword},
};
use engine_provider::{Draft, KeywordName, Provider};
use serde_json::{Value, json};

use crate::{
    provider::GraphProvider,
    test_support::{FakeRoute, RequestLog, fake_client_logged},
};

/// A live rewrite `PATCH` answer for a draft carrying the category `Engine draft probe`.
const DRAFT_PATCHED_WITH_CATEGORY: &str =
    include_str!("../tests/fixtures/mail/draft_patched_with_category.json");

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn keyword() -> Keyword {
    Keyword::new("project-x").unwrap()
}

fn tagged() -> BTreeSet<Keyword> {
    BTreeSet::from([keyword()])
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("compose-1@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    )
}

fn stored() -> ProviderKey {
    ProviderKey::new("AAMkAGold").unwrap()
}

/// A provider with the keyword's names registered, over `routes` (first match wins), and
/// the name-in-use lookup answering that no message carries one yet.
fn named(routes: Vec<(&str, FakeRoute)>) -> (GraphProvider, RequestLog) {
    let mut routes = routes;
    routes.insert(0, ("categories/any", Ok(json!({ "value": [] }))));
    let (client, _, requests) = fake_client_logged(routes);
    let provider = GraphProvider::new(client, MailboxId::try_from("folder-drafts").unwrap())
        .with_keyword_names(vec![
            KeywordName::new(keyword(), "Project X")
                .unwrap()
                .also_known_as("Projekt X")
                .also_known_as("Engine draft probe"),
        ]);
    (provider, requests)
}

/// The `categories` of every `PATCH` sent, in order.
fn categories_patched(requests: &RequestLog) -> Vec<Value> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, ..)| *method == "PATCH")
        .filter_map(|(_, _, body)| body.as_ref()?.get("categories").cloned())
        .collect()
}

#[tokio::test]
async fn a_new_draft_takes_the_category_and_reports_the_keyword_kept() {
    let (p, requests) = named(vec![
        ("/messages/AAMkAGnew", Ok(json!({}))),
        (
            "/messages",
            Ok(json!({ "id": "AAMkAGnew", "categories": [] })),
        ),
    ]);

    let receipt = p
        .put_draft(&account(), &draft(), None, &tagged())
        .await
        .unwrap();

    assert_eq!(receipt.key.as_str(), "AAMkAGnew");
    assert_eq!(receipt.keywords, tagged());
    assert_eq!(categories_patched(&requests), [json!(["Project X"])]);
}

#[tokio::test]
async fn a_re_save_in_place_takes_off_a_keyword_it_no_longer_asks_for() {
    // The stored message keeps its categories across the rewrite, so the one the person set
    // stays and the keyword's goes, under whichever of its names it was carried.
    let (p, requests) = named(vec![(
        "/messages/AAMkAGold",
        Ok(json!({ "id": "AAMkAGold", "categories": ["Blue", "projekt x"] })),
    )]);

    let receipt = p
        .put_draft(&account(), &draft(), Some(&stored()), &BTreeSet::new())
        .await
        .unwrap();

    assert_eq!(receipt.key, stored());
    assert_eq!(receipt.keywords, BTreeSet::new());
    assert_eq!(categories_patched(&requests), [json!(["Blue"])]);
}

#[tokio::test]
async fn a_re_save_that_already_carries_the_category_patches_nothing_more() {
    // The rewrite's own answer, as a live mailbox gave it, says the draft still carries the
    // category, so nothing is read again and nothing more is written.
    let (p, requests) = named(vec![(
        "/messages/AAMkAGold",
        Ok(serde_json::from_str(DRAFT_PATCHED_WITH_CATEGORY).unwrap()),
    )]);

    let receipt = p
        .put_draft(&account(), &draft(), Some(&stored()), &tagged())
        .await
        .unwrap();

    assert_eq!(receipt.keywords, tagged());
    assert_eq!(categories_patched(&requests), [] as [Value; 0]);
}

#[tokio::test]
async fn an_answer_that_does_not_say_is_read_again() {
    let (p, requests) = named(vec![
        (
            "$select=categories",
            Ok(json!({ "id": "AAMkAGold", "categories": ["Project X"] })),
        ),
        ("/messages/AAMkAGold", Ok(json!({ "id": "AAMkAGold" }))),
    ]);

    let receipt = p
        .put_draft(&account(), &draft(), Some(&stored()), &tagged())
        .await
        .unwrap();

    assert_eq!(receipt.keywords, tagged());
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|(method, url, _)| *method == "GET" && url.contains("$select=categories"))
    );
}

#[tokio::test]
async fn a_refused_category_change_leaves_the_draft_saved_and_says_so() {
    let (p, _) = named(vec![
        (
            "/messages/AAMkAGnew",
            Err((403, json!({ "error": { "code": "ErrorAccessDenied" } }))),
        ),
        (
            "/messages",
            Ok(json!({ "id": "AAMkAGnew", "categories": [] })),
        ),
    ]);

    let receipt = p
        .put_draft(&account(), &draft(), None, &tagged())
        .await
        .unwrap();

    assert_eq!(receipt.key.as_str(), "AAMkAGnew");
    assert_eq!(receipt.keywords, BTreeSet::new());
}

#[tokio::test]
async fn without_a_registered_name_nothing_is_kept_or_asked() {
    let (client, _, requests) = fake_client_logged(vec![(
        "/messages",
        Ok(json!({ "id": "AAMkAGnew", "categories": [] })),
    )]);
    let p = GraphProvider::new(client, MailboxId::try_from("folder-drafts").unwrap());
    assert!(!p.connection_info().capabilities.draft_keywords());

    let receipt = p
        .put_draft(&account(), &draft(), None, &tagged())
        .await
        .unwrap();

    assert_eq!(receipt.keywords, BTreeSet::new());
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "the create and nothing else"
    );
}
