//! Gated live pins: what the **SabreDAV** harness tells Alice about calendar homes that are
//! not hers (`docker/sabredav/delegation.php`).
//!
//! These are server facts, probed over raw `PROPFIND` because the provider does not ask for
//! them yet. They are what CalDAV shared-store discovery is designed against, and they
//! differ from Stalwart's (`stalwart-harness/tests/smoke_shared_stores.rs`) in two ways that
//! matter:
//!
//! - SabreDAV answers the CalendarServer proxy sets, so Alice's own principal names the homes
//!   delegated to her. Stalwart answers them `404`.
//! - A home she may not read is not refused: bob's answers `207`, with every property `403` inside.
//!   Stalwart answers `403` outright. So whether a home can be read is told by the calendars it
//!   lists, not by the status of the `PROPFIND`.
//!
//! Skips unless `SABREDAV_HTTP_ADDR` is set.

use stalwart_harness::{HttpResponse, dav_propfind};

fn harness() -> Option<(String, String, String)> {
    let addr = std::env::var("SABREDAV_HTTP_ADDR").ok()?;
    let user = std::env::var("SABREDAV_USER").unwrap_or_else(|_| "alice@test.local".to_owned());
    let pass = std::env::var("SABREDAV_PASS").unwrap_or_else(|_| "sabredav-alice-pw".to_owned());
    Some((addr, user, pass))
}

fn propfind(addr: &str, auth: (&str, &str), path: &str, depth: &str, body: &str) -> HttpResponse {
    dav_propfind(addr, auth, path, depth, body).expect("SabreDAV answers")
}

/// The `DAV:response` block for `href`.
fn response_for<'a>(body: &'a str, href: &str) -> Option<&'a str> {
    body.split("<d:response>")
        .find(|block| block.contains(&format!("<d:href>{href}</d:href>")))
}

/// The `200 OK` propstat inside a response block, if it has one.
fn granted(block: &str) -> Option<&str> {
    block
        .split("<d:propstat>")
        .find(|propstat| propstat.contains("HTTP/1.1 200 OK"))
}

const PRINCIPAL_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
<d:prop><c:calendar-home-set/><d:principal-collection-set/><d:group-membership/>
<cs:calendar-proxy-read-for/><cs:calendar-proxy-write-for/></d:prop></d:propfind>"#;

const HOME_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:owner/><d:current-user-privilege-set/>
<d:resourcetype/></d:prop></d:propfind>"#;

#[test]
fn sabredav_names_the_homes_delegated_to_the_user_on_her_own_principal() {
    let Some((addr, user, pass)) = harness() else {
        eprintln!("skipping sabredav delegation pins: SABREDAV_HTTP_ADDR unset");
        return;
    };
    let resp = propfind(
        &addr,
        (&user, &pass),
        &format!("/principals/{user}/"),
        "0",
        PRINCIPAL_PROPS,
    );
    assert_eq!(resp.status, 207);
    let body = resp.body_text();
    let read_for = body
        .split("calendar-proxy-read-for>")
        .nth(1)
        .unwrap_or_else(|| panic!("no proxy-read-for: {body}"));
    assert!(read_for.starts_with("<d:href>/principals/dave@test.local/</d:href>"));
    let write_for = body
        .split("calendar-proxy-write-for>")
        .nth(1)
        .unwrap_or_else(|| panic!("no proxy-write-for: {body}"));
    assert!(write_for.starts_with("<d:href>/principals/support@test.local/</d:href>"));
    // Her home set still names her own home alone; the delegated ones are reached through
    // the delegating principal.
    assert_eq!(body.matches("<d:href>/calendars/").count(), 1, "{body}");
    assert!(body.contains("<d:principal-collection-set><d:href>/principals/</d:href>"));
}

#[test]
fn sabredav_lists_every_principal_but_lets_the_user_read_only_the_delegated_homes() {
    let Some((addr, user, pass)) = harness() else {
        eprintln!("skipping sabredav delegation pins: SABREDAV_HTTP_ADDR unset");
        return;
    };
    let body = propfind(&addr, (&user, &pass), "/principals/", "1", PRINCIPAL_PROPS).body_text();
    for principal in [
        "admin",
        "alice@test.local",
        "bob@test.local",
        "dave@test.local",
    ] {
        assert!(
            response_for(&body, &format!("/principals/{principal}/")).is_some(),
            "{principal} should be listed: {body}"
        );
    }

    // Each delegated home lists a calendar collection she is granted properties on.
    for (owner, calendar, writable) in [
        ("dave@test.local", "shared", false),
        ("support@test.local", "default", true),
    ] {
        let resp = propfind(
            &addr,
            (&user, &pass),
            &format!("/calendars/{owner}/"),
            "1",
            HOME_PROPS,
        );
        assert_eq!(resp.status, 207);
        let body = resp.body_text();
        let block = response_for(&body, &format!("/calendars/{owner}/{calendar}/"))
            .unwrap_or_else(|| panic!("{owner}'s {calendar} should be listed: {body}"));
        let props = granted(block).unwrap_or_else(|| panic!("readable: {block}"));
        assert!(props.contains("<cal:calendar/>"), "{props}");
        assert!(props.contains(&format!("<d:owner><d:href>/principals/{owner}/</d:href>")));
        assert!(props.contains("<d:read/>"), "{props}");
        assert_eq!(
            props.contains("<d:write-content/>"),
            writable,
            "{owner}: {props}"
        );
    }

    // Bob delegated nothing. His home is not refused, it is emptied: `207`, and the calendar
    // inside answers its properties `403`.
    let resp = propfind(
        &addr,
        (&user, &pass),
        "/calendars/bob@test.local/",
        "1",
        HOME_PROPS,
    );
    assert_eq!(resp.status, 207);
    let body = resp.body_text();
    let calendar = response_for(&body, "/calendars/bob@test.local/default/")
        .unwrap_or_else(|| panic!("bob's calendar is listed, but closed: {body}"));
    assert!(granted(calendar).is_none(), "{calendar}");
    assert!(calendar.contains("HTTP/1.1 403 Forbidden"), "{calendar}");
}
