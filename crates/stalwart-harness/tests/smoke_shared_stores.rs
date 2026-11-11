//! What Stalwart tells the seeded account about the stores shared with it — pinned in both
//! directions, so the engine's discovery is designed against observed behaviour and a
//! server change that moves it fails here first.
//!
//! The fixture (`docker/stalwart/seed.sh`): the `support` group shares mail and a calendar
//! with every right, bob shares mail alone, dave shares one calendar alone and read-only,
//! and carol shares nothing.
//!
//! The finding these tests exist for: **a JMAP account's `accountCapabilities` does not say
//! what it holds.** Every shared account lists mail *and* calendars, yet mail-only bob
//! refuses `Calendar/get` and calendar-only dave refuses `Mailbox/get`. Over CalDAV, the
//! principal collection lists bob too, while his calendar home answers `403`.
//!
//! Skips without `STALWART_HTTP_ADDR`, like the rest of the smoke suite.

use std::time::Duration;

use serde_json::{Value, json};
use stalwart_harness::{
    CALENDAR_SHARE_OWNER, GATE_VAR, GROUP_CALENDAR_EVENT_UID, Harness, SHARED_CALENDAR_EVENT_UID,
    SHARED_CALENDAR_NAME, SHARED_GROUP_ACCOUNT,
};

const NOT_SUPPORTED: &str = "accountNotSupportedByMethod";

fn ready_harness() -> Option<Harness> {
    let Some(harness) = Harness::from_env() else {
        let test = std::thread::current().name().unwrap_or("test").to_owned();
        eprintln!("skipping `{test}`: set {GATE_VAR} (and run docker compose) to exercise it");
        return None;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("Stalwart should become ready");
    Some(harness)
}

/// The seeded account's session entries, keyed by account name.
fn accounts_by_name(h: &Harness) -> Vec<(String, String, Value)> {
    let session = h.jmap_session().expect("JMAP session");
    session["accounts"]
        .as_object()
        .expect("accounts object")
        .iter()
        .map(|(id, account)| {
            let name = account["name"].as_str().expect("named account").to_owned();
            (name, id.clone(), account.clone())
        })
        .collect()
}

fn account_id(h: &Harness, name: &str) -> String {
    accounts_by_name(h)
        .into_iter()
        .find(|(n, ..)| n == name)
        .unwrap_or_else(|| panic!("the session lists no account named {name}"))
        .1
}

/// One JMAP method call as the seeded account; returns its `[name, arguments]`.
fn call(h: &Harness, method: &str, args: &Value) -> (String, Value) {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail",
                  "urn:ietf:params:jmap:calendars"],
        "methodCalls": [[method, args, "c0"]],
    });
    let resp = h
        .jmap_post(body.to_string().as_bytes())
        .expect("JMAP answers");
    assert_eq!(resp.status, 200, "{}", resp.body_text());
    let parsed: Value = serde_json::from_slice(&resp.body).expect("JMAP JSON");
    let response = &parsed["methodResponses"][0];
    (
        response[0].as_str().expect("method name").to_owned(),
        response[1].clone(),
    )
}

/// The error type a method answered, or `None` when it answered normally.
fn refusal(h: &Harness, method: &str, account: &str) -> Option<String> {
    let (name, args) = call(h, method, &json!({ "accountId": account }));
    (name == "error").then(|| args["type"].as_str().expect("error type").to_owned())
}

#[test]
fn every_shared_jmap_account_claims_both_mail_and_calendars() {
    let Some(h) = ready_harness() else {
        return;
    };
    let owner = h.read_only_share_owner().address.clone();
    for name in [SHARED_GROUP_ACCOUNT, &owner, CALENDAR_SHARE_OWNER] {
        let (_, _, account) = accounts_by_name(&h)
            .into_iter()
            .find(|(n, ..)| n == name)
            .unwrap_or_else(|| panic!("the session lists no account named {name}"));
        assert_eq!(account["isPersonal"], json!(false), "{name}");
        let caps = account["accountCapabilities"].as_object().expect("caps");
        for capability in [
            "urn:ietf:params:jmap:mail",
            "urn:ietf:params:jmap:calendars",
        ] {
            assert!(caps.contains_key(capability), "{name} lacks {capability}");
        }
    }
    // Carol shares nothing, so her account is not in the session at all.
    let carol = &h.scratch[1].address;
    assert!(accounts_by_name(&h).iter().all(|(n, ..)| n != carol));
}

/// The capability list above overclaims, in both directions: the method answers what it
/// does not.
#[test]
fn a_shared_jmap_account_answers_only_for_what_was_shared() {
    let Some(h) = ready_harness() else {
        return;
    };
    let bob = account_id(&h, &h.read_only_share_owner().address);
    let dave = account_id(&h, CALENDAR_SHARE_OWNER);
    let group = account_id(&h, SHARED_GROUP_ACCOUNT);

    // Mail-only bob: mail answers, calendars refuse.
    assert_eq!(refusal(&h, "Mailbox/get", &bob), None);
    assert_eq!(
        refusal(&h, "Calendar/get", &bob).as_deref(),
        Some(NOT_SUPPORTED)
    );
    // Calendar-only dave: the mirror.
    assert_eq!(refusal(&h, "Calendar/get", &dave), None);
    assert_eq!(
        refusal(&h, "Mailbox/get", &dave).as_deref(),
        Some(NOT_SUPPORTED)
    );
    // The group: both.
    assert_eq!(refusal(&h, "Mailbox/get", &group), None);
    assert_eq!(refusal(&h, "Calendar/get", &group), None);
}

#[test]
fn a_shared_calendar_carries_the_rights_it_was_shared_with() {
    let Some(h) = ready_harness() else {
        return;
    };
    let properties = json!(["name", "myRights", "isSubscribed"]);
    let dave = account_id(&h, CALENDAR_SHARE_OWNER);
    let (_, got) = call(
        &h,
        "Calendar/get",
        &json!({ "accountId": dave, "properties": properties }),
    );
    let list = got["list"].as_array().expect("list");
    // Only the calendar dave shared; his default calendar stays private.
    assert_eq!(list.len(), 1, "{got}");
    assert_eq!(list[0]["name"], json!(SHARED_CALENDAR_NAME));
    assert_eq!(list[0]["myRights"]["mayReadItems"], json!(true));
    for right in ["mayWriteAll", "mayWriteOwn", "mayRSVP", "mayDelete"] {
        assert_eq!(list[0]["myRights"][right], json!(false), "{right}: {got}");
    }
    // A share is not subscribed to until its grantee says so.
    assert_eq!(list[0]["isSubscribed"], json!(false));

    let group = account_id(&h, SHARED_GROUP_ACCOUNT);
    let (_, got) = call(
        &h,
        "Calendar/get",
        &json!({ "accountId": group, "properties": properties }),
    );
    let list = got["list"].as_array().expect("list");
    assert_eq!(list.len(), 1, "{got}");
    for right in ["mayReadItems", "mayWriteAll", "mayRSVP", "mayDelete"] {
        assert_eq!(list[0]["myRights"][right], json!(true), "{right}: {got}");
    }
}

#[test]
fn each_shared_calendar_holds_its_own_seeded_event() {
    let Some(h) = ready_harness() else {
        return;
    };
    for (store, uid) in [
        (CALENDAR_SHARE_OWNER, SHARED_CALENDAR_EVENT_UID),
        (SHARED_GROUP_ACCOUNT, GROUP_CALENDAR_EVENT_UID),
    ] {
        let account = account_id(&h, store);
        let (_, got) = call(
            &h,
            "CalendarEvent/get",
            &json!({ "accountId": account, "properties": ["uid"] }),
        );
        let uids: Vec<&str> = got["list"]
            .as_array()
            .expect("list")
            .iter()
            .filter_map(|event| event["uid"].as_str())
            .collect();
        assert!(uids.contains(&uid), "{store}: {got}");
    }
}

/// `ParticipantIdentity/get` answers for the **user**, whichever account it is asked on:
/// the group's and dave's accounts both return alice's two identities and never name dave.
/// So in a shared store, the identities do not say who the store's owner is.
#[test]
fn participant_identities_belong_to_the_user_not_the_account() {
    let Some(h) = ready_harness() else {
        return;
    };
    for store in [CALENDAR_SHARE_OWNER, SHARED_GROUP_ACCOUNT] {
        let account = account_id(&h, store);
        let (_, got) = call(
            &h,
            "ParticipantIdentity/get",
            &json!({ "accountId": account }),
        );
        let mut addresses: Vec<&str> = got["list"]
            .as_array()
            .expect("list")
            .iter()
            .filter_map(|identity| identity["calendarAddress"].as_str())
            .collect();
        addresses.sort_unstable();
        assert_eq!(
            addresses,
            [
                format!("mailto:{}", h.account),
                format!("mailto:{SHARED_GROUP_ACCOUNT}")
            ],
            "{store}: {got}"
        );
    }
    // And where calendars do not answer, neither do identities.
    let bob = account_id(&h, &h.read_only_share_owner().address);
    assert_eq!(
        refusal(&h, "ParticipantIdentity/get", &bob).as_deref(),
        Some(NOT_SUPPORTED)
    );
}

/// The `DAV:response` block for `href` in a multistatus body.
fn response_for<'a>(body: &'a str, href: &str) -> Option<&'a str> {
    body.split("<D:response>")
        .find(|block| block.contains(&format!("<D:href>{href}</D:href>")))
}

fn encoded(address: &str) -> String {
    address.replace('@', "%40")
}

const PRINCIPAL_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
<d:prop><c:calendar-home-set/><d:principal-collection-set/><d:group-membership/>
<cs:calendar-proxy-read-for/><cs:calendar-proxy-write-for/></d:prop></d:propfind>"#;

/// Alice's own principal points at her home alone, and offers none of the routes a client
/// could follow to another one: no proxy sets (`404`), and an empty `group-membership`
/// although she is in the support group.
#[test]
fn the_users_principal_names_only_her_own_home() {
    let Some(h) = ready_harness() else {
        return;
    };
    let me = format!("/dav/pal/{}/", encoded(&h.account));
    let resp = h.dav_propfind_body(&me, "0", PRINCIPAL_PROPS).unwrap();
    assert_eq!(resp.status, 207);
    let body = resp.body_text();
    let homes: Vec<&str> = body.matches("<D:href>/dav/cal/").collect();
    assert_eq!(homes.len(), 1, "one calendar-home-set entry: {body}");
    assert!(body.contains(&format!(
        "<D:href>/dav/cal/{}/</D:href>",
        encoded(&h.account)
    )));
    assert!(body.contains("<D:principal-collection-set><D:href>/dav/pal/</D:href>"));
    assert!(body.contains("<D:group-membership/>"), "{body}");
    let missing = body
        .split("<D:propstat>")
        .find(|block| block.contains("404 Not Found"))
        .unwrap_or_else(|| panic!("the proxy sets should be 404: {body}"));
    assert!(missing.contains("calendar-proxy-read-for"), "{body}");
    assert!(missing.contains("calendar-proxy-write-for"), "{body}");
}

/// The principal collection lists every principal alice holds *any* share from — bob for
/// his INBOX included — so a listed principal is not one whose calendars she can read. Only
/// asking the home tells: `403` for bob and for carol (whom it does not list), `207` for the
/// two that shared a calendar.
#[test]
fn the_principal_collection_lists_more_than_can_be_read() {
    let Some(h) = ready_harness() else {
        return;
    };
    let resp = h
        .dav_propfind_body("/dav/pal/", "1", PRINCIPAL_PROPS)
        .unwrap();
    assert_eq!(resp.status, 207);
    let body = resp.body_text();
    let bob = h.read_only_share_owner().address.clone();
    let carol = h.scratch[1].address.clone();
    for listed in [&h.account, &bob, CALENDAR_SHARE_OWNER, SHARED_GROUP_ACCOUNT] {
        let principal = format!("/dav/pal/{}/", encoded(listed));
        let block = response_for(&body, &principal)
            .unwrap_or_else(|| panic!("{principal} should be listed: {body}"));
        // Each names its own home.
        assert!(block.contains(&format!("<D:href>/dav/cal/{}/</D:href>", encoded(listed))));
    }
    assert!(response_for(&body, &format!("/dav/pal/{}/", encoded(&carol))).is_none());

    for (owner, status) in [
        (bob.as_str(), 403),
        (carol.as_str(), 403),
        (CALENDAR_SHARE_OWNER, 207),
        (SHARED_GROUP_ACCOUNT, 207),
    ] {
        let home = format!("/dav/cal/{}/", encoded(owner));
        let got = h.dav_propfind_body(&home, "1", OWNER_PROPS).unwrap().status;
        assert_eq!(got, status, "{home}");
    }
}

const OWNER_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:owner/><d:current-user-privilege-set/>
<d:resourcetype/></d:prop></d:propfind>"#;

/// A shared home holds only the collections shared from it, each naming its owner with
/// `DAV:owner` and carrying the rights it was shared with.
#[test]
fn a_shared_home_shows_only_what_was_shared_and_whose_it_is() {
    let Some(h) = ready_harness() else {
        return;
    };
    let dave = encoded(CALENDAR_SHARE_OWNER);
    let resp = h
        .dav_propfind_body(&format!("/dav/cal/{dave}/"), "1", OWNER_PROPS)
        .unwrap();
    let body = resp.body_text();
    let shared = response_for(&body, &format!("/dav/cal/{dave}/shared/"))
        .unwrap_or_else(|| panic!("dave's shared calendar should be listed: {body}"));
    assert!(response_for(&body, &format!("/dav/cal/{dave}/default/")).is_none());
    assert!(shared.contains(&format!("<D:owner><D:href>/dav/pal/{dave}/</D:href>")));
    assert!(shared.contains("<D:read/>"), "{shared}");
    assert!(!shared.contains("<D:write"), "read-only: {shared}");

    let group = encoded(SHARED_GROUP_ACCOUNT);
    let resp = h
        .dav_propfind_body(&format!("/dav/cal/{group}/"), "1", OWNER_PROPS)
        .unwrap();
    let body = resp.body_text();
    let calendar = response_for(&body, &format!("/dav/cal/{group}/default/"))
        .unwrap_or_else(|| panic!("the group's calendar should be listed: {body}"));
    assert!(calendar.contains(&format!("<D:owner><D:href>/dav/pal/{group}/</D:href>")));
    assert!(calendar.contains("<D:write-content/>"), "{calendar}");
}
