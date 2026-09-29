//! Which data domains a session offers: read from the primary account of each capability and
//! that account's `accountCapabilities` (RFC 8620 §2), not from the server's own list.

use serde_json::json;

use super::*;

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
const CONTACTS: &str = "urn:ietf:params:jmap:contacts";

fn parse(doc: &Value) -> Session {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    Session::parse(doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap()
}

/// A server that supports all four domains, with the given primary accounts and accounts.
fn server(primary: &Value, accounts: &Value) -> Value {
    json!({
        "capabilities": { CORE: {}, MAIL: {}, SUBMISSION: {}, CALENDARS: {}, CONTACTS: {} },
        "primaryAccounts": primary,
        "accounts": accounts,
        "apiUrl": "https://mail.test.local/jmap/",
    })
}

fn account(caps: &[&str]) -> Value {
    let caps: serde_json::Map<String, Value> = caps
        .iter()
        .map(|urn| ((*urn).to_owned(), json!({})))
        .collect();
    json!({ "name": "alice@test.local", "isReadOnly": false, "accountCapabilities": caps })
}

#[test]
fn a_server_with_calendars_does_not_give_them_to_an_account_without_them() {
    let doc = server(
        &json!({ MAIL: "c", SUBMISSION: "c" }),
        &json!({ "c": account(&[CORE, MAIL, SUBMISSION]) }),
    );
    let caps = parse(&doc).capabilities();
    assert!(caps.mail() && caps.submission());
    assert!(
        !caps.calendars(),
        "the server lists calendars, the account does not"
    );
    assert!(!caps.contacts());
    assert!(!caps.calendar_writes() && !caps.contact_writes());
}

#[test]
fn a_primary_account_that_does_not_list_the_capability_does_not_offer_it() {
    // RFC 8620 §2 keys `primaryAccounts` by the URIs in `accountCapabilities`, so this is a
    // contradiction; the account's own list is the narrower claim and wins.
    let doc = server(
        &json!({ MAIL: "c", SUBMISSION: "c", CALENDARS: "c" }),
        &json!({ "c": account(&[CORE, MAIL, SUBMISSION]) }),
    );
    assert!(!parse(&doc).capabilities().calendars());
}

#[test]
fn each_domain_is_read_from_its_own_primary_account() {
    let doc = server(
        &json!({ MAIL: "c", SUBMISSION: "c", CALENDARS: "d", CONTACTS: "d" }),
        &json!({
            "c": account(&[CORE, MAIL, SUBMISSION]),
            "d": account(&[CORE, CALENDARS, CONTACTS]),
        }),
    );
    let session = parse(&doc);
    let caps = session.capabilities();
    assert!(caps.mail() && caps.submission() && caps.calendars() && caps.contacts());
    assert!(caps.calendar_writes() && caps.contact_writes());
    assert_eq!(session.mail_account_id().unwrap(), "c");
    assert_eq!(session.calendar_account_id().unwrap(), "d");
    assert_eq!(session.contact_account_id().unwrap(), "d");
}

#[test]
fn an_account_with_calendars_and_no_mail_offers_only_calendars() {
    let doc = server(
        &json!({ CALENDARS: "d" }),
        &json!({ "d": account(&[CORE, CALENDARS]) }),
    );
    let caps = parse(&doc).capabilities();
    assert!(caps.calendars());
    assert!(!caps.mail() && !caps.submission() && !caps.contacts());
}

#[test]
fn a_capability_with_no_primary_account_is_not_offered() {
    // The adapter addresses every call to the capability's primary account, so a domain with
    // none could only fail at the first call.
    let doc = server(
        &json!({ MAIL: "c" }),
        &json!({ "c": account(&[CORE, MAIL, CALENDARS]) }),
    );
    let caps = parse(&doc).capabilities();
    assert!(caps.mail());
    assert!(!caps.calendars() && !caps.submission());
}

#[test]
fn an_account_without_an_account_capabilities_map_is_read_by_the_server_list() {
    // `accountCapabilities` is required (RFC 8620 §2); a document without it is read as it
    // always was, by the server's list, provided the capability has a primary account.
    let doc = server(
        &json!({ MAIL: "c", CALENDARS: "c" }),
        &json!({ "c": { "name": "alice@test.local" } }),
    );
    let caps = parse(&doc).capabilities();
    assert!(caps.mail() && caps.calendars());
    assert!(!caps.contacts(), "no primary contacts account");
}

#[test]
fn a_capability_the_server_does_not_list_is_not_offered_even_if_an_account_claims_it() {
    let doc = json!({
        "capabilities": { CORE: {}, MAIL: {} },
        "primaryAccounts": { MAIL: "c", CALENDARS: "c" },
        "accounts": { "c": account(&[CORE, MAIL, CALENDARS]) },
        "apiUrl": "https://mail.test.local/jmap/",
    });
    assert!(!parse(&doc).capabilities().calendars());
}
