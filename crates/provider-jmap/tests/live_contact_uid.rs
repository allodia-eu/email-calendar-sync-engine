//! Gated live probe: what Stalwart keeps of a `ContactCard`'s `uid`. Skips with no
//! `STALWART_HTTP_ADDR`.
//!
//! RFC 9553 §2.1.9 makes a Card's `uid` mandatory, and RFC 9610 does not ask a server to supply a
//! missing one, so the adapter always sends one (`contact_write::create_object`), minting it for a
//! draft without one; a retried create of that draft sends the same `uid` again, so the second
//! probe records what a card with a `uid` the account already holds meets. Before recording
//! what the server does with a Card sent *without* one, both directions are pinned with raw calls,
//! so the answer cannot come from a request that never asked: a card created with a `uid` and one
//! created without, each read back with `properties` left null (every property) and with
//! `["uid"]` named. Nothing about a card is printed but whether it has a `uid`. Stalwart stores a
//! card sent without one as it is, with no `uid` (`docs/spec-deviations.md`, STALWART-11).

use serde_json::{Value, json};
use stalwart_harness::Harness;

const CONTACTS: &str = "urn:ietf:params:jmap:contacts";

/// One raw JMAP call, answered with its result arguments.
fn call(harness: &Harness, method: &str, args: &Value) -> Value {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", CONTACTS],
        "methodCalls": [[method, args, "0"]],
    });
    let response = harness
        .jmap_post(body.to_string().as_bytes())
        .expect("jmap post");
    let parsed: Value = serde_json::from_str(&response.body_text()).expect("json");
    let answer = &parsed["methodResponses"][0];
    assert_eq!(answer[0], method, "{method} answered an error");
    answer[1].clone()
}

/// Whether each of `ids` reads back with a `uid`, and which, asked with `properties`.
fn uids(harness: &Harness, account: &str, ids: &[&str], properties: &Value) -> Vec<Option<String>> {
    let got = call(
        harness,
        "ContactCard/get",
        &json!({ "accountId": account, "ids": ids, "properties": properties }),
    );
    ids.iter()
        .map(|id| {
            let card = got["list"]
                .as_array()
                .expect("a list")
                .iter()
                .find(|card| card["id"] == *id)
                .expect("the card reads back");
            card.get("uid").and_then(Value::as_str).map(str::to_owned)
        })
        .collect()
}

/// An address book of `account` the harness account may add cards to.
fn writable_book(harness: &Harness, account: &str) -> String {
    let books = call(
        harness,
        "AddressBook/get",
        &json!({ "accountId": account, "ids": null }),
    );
    books["list"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|book| book["myRights"]["mayAddItems"] != false)
        .and_then(|book| book["id"].as_str())
        .expect("a writable address book")
        .to_owned()
}

#[test]
fn a_card_created_without_a_uid_is_stored_without_one_and_one_created_with_keeps_it() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping contact uid probe: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let session = harness.jmap_session().expect("session");
    let account = session["primaryAccounts"][CONTACTS]
        .as_str()
        .expect("a contacts account")
        .to_owned();
    let book = writable_book(&harness, &account);

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let explicit = format!("urn:uuid:live-uid-probe-{nonce}");
    let card = |name: &str| {
        json!({
            "@type": "Card",
            "version": "1.0",
            "kind": "individual",
            "name": { "full": format!("Uid Probe {name} {nonce}") },
            "addressBookIds": { book.as_str(): true },
        })
    };
    let mut with = card("With");
    with["uid"] = json!(explicit);
    let created = call(
        &harness,
        "ContactCard/set",
        &json!({
            "accountId": account,
            "create": { "without": card("Without"), "with": with },
        }),
    );
    let id = |key: &str| {
        created["created"][key]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("`{key}` was not created"))
            .to_owned()
    };
    let (without, with) = (id("without"), id("with"));
    let created_reports_uid = created["created"]["without"].get("uid").is_some();
    let all = uids(&harness, &account, &[&without, &with], &Value::Null);
    let named = uids(&harness, &account, &[&without, &with], &json!(["uid"]));
    call(
        &harness,
        "ContactCard/set",
        &json!({ "accountId": account, "destroy": [without, with] }),
    );

    eprintln!(
        "without a uid: `created` reports one: {created_reports_uid}; read with every property: \
         {}; read naming `uid`: {}",
        all[0].is_some(),
        named[0].is_some()
    );
    // The control: a `uid` the card was given is kept, and both reads return it.
    assert_eq!(all[1].as_deref(), Some(explicit.as_str()));
    assert_eq!(named[1].as_deref(), Some(explicit.as_str()));
    // A card sent without one is stored without one: the server neither refuses the create nor
    // assigns a `uid`, by either read (`docs/spec-deviations.md`, STALWART-11).
    assert!(!created_reports_uid);
    assert_eq!(all[0], None);
    assert_eq!(named[0], None);
}

/// What a second card with a `uid` the account already holds meets. RFC 9610 §3: "there MUST NOT
/// be more than one ContactCard with the same uid in an Account". A retried create of a draft
/// without a `uid` sends the same minted one, so this is what such a retry meets. Both
/// directions in one request: a card with a fresh `uid` is created beside it, so a refusal is
/// about the `uid` and not the request. Prints only which happened and the SetError type.
#[test]
fn a_second_card_with_a_uid_the_account_holds_is_refused() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping duplicate uid probe: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let session = harness.jmap_session().expect("session");
    let account = session["primaryAccounts"][CONTACTS]
        .as_str()
        .expect("a contacts account")
        .to_owned();
    let book = writable_book(&harness, &account);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let card = |name: &str, uid: &str| {
        json!({
            "@type": "Card",
            "version": "1.0",
            "kind": "individual",
            "uid": uid,
            "name": { "full": format!("Uid Probe {name} {nonce}") },
            "addressBookIds": { book.as_str(): true },
        })
    };
    let held = format!("urn:uuid:live-uid-held-{nonce}");
    let fresh = format!("urn:uuid:live-uid-fresh-{nonce}");
    let first = call(
        &harness,
        "ContactCard/set",
        &json!({ "accountId": account, "create": { "first": card("First", &held) } }),
    );
    let mut created: Vec<String> = first["created"]["first"]["id"]
        .as_str()
        .map(str::to_owned)
        .into_iter()
        .collect();
    assert_eq!(created.len(), 1, "the first card is created");
    let second = call(
        &harness,
        "ContactCard/set",
        &json!({
            "accountId": account,
            "create": {
                "again": card("Again", &held),
                "fresh": card("Fresh", &fresh),
            },
        }),
    );
    for key in ["again", "fresh"] {
        if let Some(id) = second["created"][key]["id"].as_str() {
            created.push(id.to_owned());
        }
    }
    call(
        &harness,
        "ContactCard/set",
        &json!({ "accountId": account, "destroy": created }),
    );

    let again_created = second["created"].get("again").is_some();
    let again_refusal = second["notCreated"]["again"]["type"]
        .as_str()
        .map(str::to_owned);
    eprintln!(
        "a second card with a held uid: created: {again_created}; refused as: {again_refusal:?}"
    );
    assert!(
        second["created"].get("fresh").is_some(),
        "the control: a fresh uid in the same request is created"
    );
    assert!(!again_created, "the held uid is not stored twice");
    assert!(again_refusal.is_some(), "and the create is refused");
}
