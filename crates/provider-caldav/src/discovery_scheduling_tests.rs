//! What discovery reads about scheduling: the calendar-auto-schedule class off the calendar
//! home's own `OPTIONS` header, and the calendar user's addresses off the principal. Split from
//! `discovery_tests.rs` by responsibility.

use engine_provider::IgnoreConnectSteps;

use super::*;
use crate::{
    test_support::{Replay, ok, options},
    transport::HttpResponse,
};

/// The `DAV:` header Stalwart really returns for an `OPTIONS` on the calendar home.
const STALWART_DAV: &str = include_str!("../tests/fixtures/options-dav-stalwart.txt");
/// The same header from SabreDAV, which serves calendar access only.
const SABREDAV_DAV: &str = include_str!("../tests/fixtures/options-dav-sabredav.txt");

#[tokio::test]
async fn scheduling_is_read_off_the_servers_own_options_header() {
    // The two harness servers, verbatim. Stalwart advertises RFC 6638 §2's
    // `calendar-auto-schedule`; SabreDAV — calendar access only — does not, and both
    // list `calendar-access`, so nothing but the scheduling token separates them.
    let stalwart = Replay::new(vec![options(Some(STALWART_DAV))]);
    assert!(
        discover_scheduling(&stalwart, "/dav/cal/alice%40test.local/")
            .await
            .unwrap()
    );
    assert_eq!(stalwart.seen()[0].0, DavMethod::Options);

    let sabredav = Replay::new(vec![options(Some(SABREDAV_DAV))]);
    assert!(
        !discover_scheduling(&sabredav, "/calendars/alice@test.local/")
            .await
            .unwrap(),
        "a plain CalDAV server must not be reported as scheduling: an RSVP there \
         rewrites PARTSTAT and tells the organizer nothing"
    );
}

#[tokio::test]
async fn scheduling_is_asked_of_the_calendar_home_not_the_connection_root() {
    // The `DAV:` header belongs to a DAV resource, and a server's site root need not
    // be one — Stalwart's answers `302` to its web UI with no header at all. So the
    // target is the home discovery just resolved.
    let exec = Replay::new(vec![options(Some(STALWART_DAV))]);
    discover_scheduling(&exec, "/dav/cal/alice%40test.local/")
        .await
        .unwrap();
    assert_eq!(exec.seen()[0].1, "/dav/cal/alice%40test.local/");
}

#[tokio::test]
async fn a_server_that_reports_no_scheduling_class_is_not_scheduling() {
    // Three ways to say "no", none of which may fail the connect: no `DAV` header at
    // all, a header listing only access classes, and a server that refuses `OPTIONS`
    // outright. An account whose calendars read and write perfectly well must not be
    // unusable because its server declined one discovery question.
    for response in [
        options(None),
        options(Some("1, 3, calendar-access")),
        HttpResponse {
            status: 405,
            body: String::new(),
            location: None,
            etag: None,
            dav: None,
            retry_after: None,
        },
    ] {
        let exec = Replay::new(vec![response]);
        assert!(!discover_scheduling(&exec, "/dav/cal/").await.unwrap());
    }
}

#[tokio::test]
async fn a_compliance_class_is_matched_whole_and_case_insensitively() {
    // Servers choose their own case and spacing, so the match is on the trimmed token
    // — but it is a *whole* token: a vendor class that merely contains the RFC 6638
    // spelling is a different feature, and reading it as scheduling would promise a
    // host that iTIP leaves the server when it does not.
    let cased = Replay::new(vec![options(Some("1, 3, CALENDAR-AUTO-SCHEDULE"))]);
    assert!(discover_scheduling(&cased, "/dav/cal/").await.unwrap());

    let lookalike = Replay::new(vec![options(Some("1, 3, x-calendar-auto-schedule"))]);
    assert!(!discover_scheduling(&lookalike, "/dav/cal/").await.unwrap());
}

/// Stalwart's principal, verbatim: the only resource that carries the RFC 6638 properties.
const STALWART_PRINCIPAL: &str = include_str!("../tests/fixtures/principal-addresses.xml");

/// Stalwart's answer at `/dav/cal/`: the home and the principal, with the address set `404`
/// because this resource is not the principal.
const STALWART_CAL_ROOT: &str = r#"<?xml version="1.0" encoding="UTF-8"?><D:multistatus xmlns:D="DAV:" xmlns:A="urn:ietf:params:xml:ns:caldav"><D:response><D:href>/dav/cal/</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/dav/pal/alice%40test.local/</D:href></D:current-user-principal><A:calendar-home-set><D:href>/dav/cal/alice%40test.local/</D:href></A:calendar-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><A:calendar-user-address-set/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response></D:multistatus>"#;

/// A start URL that names only the principal, for the two-step flow.
fn root_naming_the_principal() -> HttpResponse {
    ok(
        "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principals/dennis/</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    )
}

/// A principal holding the home and, when given, this address-set property.
fn principal_with(address_set: &str) -> HttpResponse {
    ok(&format!(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/principals/dennis/</D:href><D:propstat><D:prop><C:calendar-home-set><D:href>/calendars/dennis/</D:href></C:calendar-home-set>{address_set}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"
    ))
}

#[tokio::test]
async fn the_two_step_flow_reads_the_address_set_off_the_principal() {
    let set = "<C:calendar-user-address-set><D:href>mailto:dennis@example.org</D:href><D:href>urn:uuid:7</D:href><D:href>mailto:d@example.org</D:href></C:calendar-user-address-set>";
    let exec = Replay::new(vec![root_naming_the_principal(), principal_with(set)]);
    let home = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    assert_eq!(home.href, "/calendars/dennis/");
    assert_eq!(home.principal.as_deref(), Some("/principals/dennis/"));
    assert_eq!(
        home.addresses,
        Some(CalendarUserAddresses::Known(vec![
            "dennis@example.org".to_owned(),
            "d@example.org".to_owned(),
        ]))
    );
}

#[tokio::test]
async fn a_principal_without_an_address_set_is_not_enabled_for_scheduling() {
    // RFC 6638 §2.4.1: the property is required on a principal that schedules, so its
    // absence from the principal itself is an answer.
    let exec = Replay::new(vec![root_naming_the_principal(), principal_with("")]);
    let home = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    assert_eq!(home.addresses, Some(CalendarUserAddresses::NotEnabled));
}

#[tokio::test]
async fn a_home_found_before_the_principal_leaves_the_address_set_unread() {
    // A `404` at a resource that is not the principal says nothing about the user, so
    // discovery does not read it as "not enabled", and it spends no extra request at connect.
    let exec = Replay::new(vec![ok(STALWART_CAL_ROOT)]);
    let home = discover_home(&exec, "/dav/cal/", &IgnoreConnectSteps)
        .await
        .unwrap();
    assert_eq!(home.href, "/dav/cal/alice%40test.local/");
    assert_eq!(
        home.principal.as_deref(),
        Some("/dav/pal/alice%40test.local/")
    );
    assert_eq!(home.addresses, None);
    assert_eq!(exec.seen().len(), 1);
}

#[tokio::test]
async fn the_principal_is_asked_for_its_address_set_on_demand() {
    let exec = Replay::new(vec![ok(STALWART_PRINCIPAL)]);
    let addresses = principal_addresses(&exec, "/dav/pal/alice%40test.local/")
        .await
        .unwrap();
    assert_eq!(
        addresses,
        CalendarUserAddresses::Known(vec!["alice@test.local".to_owned()])
    );
    assert_eq!(
        exec.seen()[0],
        (
            DavMethod::Propfind,
            "/dav/pal/alice%40test.local/".to_owned()
        )
    );

    let without = Replay::new(vec![principal_with("")]);
    assert_eq!(
        principal_addresses(&without, "/principals/dennis/")
            .await
            .unwrap(),
        CalendarUserAddresses::NotEnabled
    );
}
