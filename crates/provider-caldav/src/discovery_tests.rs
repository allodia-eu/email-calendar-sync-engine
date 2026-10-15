use std::sync::Mutex;

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

/// A `307` to `location`.
fn redirect_to(location: &str) -> HttpResponse {
    HttpResponse {
        status: 307,
        body: String::new(),
        location: Some(location.to_owned()),
        etag: None,
        dav: None,
        retry_after: None,
    }
}

/// Records the hops an observer sees, as `from -> to` pairs.
#[derive(Default)]
struct Hops(Mutex<Vec<String>>);

impl ConnectObserver for Hops {
    fn step(&self, step: &ConnectStep<'_>) {
        if let ConnectStep::Redirected { from, to, .. } = step {
            self.0.lock().unwrap().push(format!("{from} -> {to}"));
        }
    }
}

#[tokio::test]
async fn every_hop_of_a_redirect_chain_is_reported_in_order() {
    // Two hops before the principal answers, so the steps are a *sequence*.
    let exec = Replay::new(vec![
        redirect_to("/dav"),
        redirect_to("/dav/cal"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    let hops = Hops::default();
    discover_home(&exec, "/.well-known/caldav", &hops)
        .await
        .unwrap();
    assert_eq!(
        *hops.0.lock().unwrap(),
        ["/.well-known/caldav -> /dav", "/dav -> /dav/cal"]
    );
}

#[tokio::test]
async fn a_relative_redirect_after_an_origin_change_stays_on_the_new_origin() {
    // RFC 6764 discovery starts on the account's domain and a hosted provider may
    // send it to the host actually serving DAV. The hop after that answers with a
    // bare path, which belongs to the *new* origin: left as a path it would be
    // resolved onto the connection base, which is a different server.
    let exec = Replay::new(vec![
        redirect_to("https://dav.example.net/principals/u/"),
        redirect_to("/dav/calendars/u/"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    let seen = exec.seen();
    assert_eq!(seen[0].1, "/.well-known/caldav");
    assert_eq!(seen[1].1, "https://dav.example.net/principals/u/");
    assert_eq!(seen[2].1, "https://dav.example.net/dav/calendars/u/");
}

/// A `Replay` whose connection refuses to move — what the live client answers when
/// the hop names a plaintext origin (`DavExecutor::adopt_origin`).
struct RefusesToMove(Replay);

#[async_trait::async_trait]
impl DavExecutor for RefusesToMove {
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError> {
        self.0.send(method, href, depth, body).await
    }

    async fn send_write(
        &self,
        request: crate::transport::WriteRequest,
    ) -> Result<HttpResponse, CalDavError> {
        self.0.send_write(request).await
    }

    fn adopt_origin(&self, _url: &str) -> bool {
        false
    }
}

#[tokio::test]
async fn a_refused_origin_change_fails_the_walk_rather_than_following_it() {
    // Discovery starts at a bare well-known path, which names no scheme, so only the
    // connection can tell that the hop gives up TLS. A refusal must stop the walk:
    // every request it would go on to make carries the account's credentials.
    let exec = RefusesToMove(Replay::new(vec![
        redirect_to("http://dav.example.net/principals/u/"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]));
    let err = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("left TLS"), "unexpected: {err}");
    // And nothing was sent to the plaintext host.
    assert_eq!(exec.0.seen().len(), 1);
}

#[tokio::test]
async fn a_redirect_target_carrying_credentials_is_scrubbed() {
    // A `Location` is server-controlled and may carry userinfo; these steps are
    // built to be logged (`north-star.md`).
    let exec = Replay::new(vec![
        redirect_to("https://alice:hunter2@dav.example.com/cal"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    let hops = Hops::default();
    discover_home(&exec, "/.well-known/caldav", &hops)
        .await
        .unwrap();
    assert_eq!(
        *hops.0.lock().unwrap(),
        ["/.well-known/caldav -> https://dav.example.com/cal"]
    );
}

#[tokio::test]
async fn the_principal_second_step_is_not_a_redirect() {
    // The RFC 6764 §6 two-step flow is two PROPFINDs of *different* resources, not
    // one resource moving — so it emits no hop.
    let root = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principals/users/dennis/</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    );
    let principal = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/principals/users/dennis/</D:href><D:propstat><D:prop><C:calendar-home-set><D:href>/calendars/dennis/</D:href></C:calendar-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    );
    let hops = Hops::default();
    let exec = Replay::new(vec![root, principal]);
    discover_home(&exec, "/.well-known/caldav", &hops)
        .await
        .unwrap();
    assert!(hops.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn follows_a_redirect_then_reads_the_home() {
    let redirect = HttpResponse {
        status: 307,
        body: String::new(),
        location: Some("/dav/cal".to_owned()),
        etag: None,
        dav: None,
        retry_after: None,
    };
    let exec = Replay::new(vec![
        redirect,
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    let home = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    assert_eq!(home.href, "/dav/cal/alice%40test.local/");
    // Two requests: the well-known, then the redirect target.
    let seen = exec.seen();
    assert_eq!(seen[0].1, "/.well-known/caldav");
    assert_eq!(seen[1].1, "/dav/cal");
}

#[tokio::test]
async fn discovers_the_home_in_two_steps_via_the_principal() {
    // The RFC-correct shape (e.g. Soverin): the start URL returns only the
    // current-user-principal (the home-set comes back 404 there), so discovery
    // must PROPFIND the principal for the calendar-home-set.
    let root = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principals/users/dennis/</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><C:calendar-home-set/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response></D:multistatus>",
    );
    let principal = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/principals/users/dennis/</D:href><D:propstat><D:prop><C:calendar-home-set><D:href>/calendars/dennis/</D:href></C:calendar-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    );
    let exec = Replay::new(vec![root, principal]);

    let home = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    assert_eq!(home.href, "/calendars/dennis/");
    let seen = exec.seen();
    assert_eq!(seen[0].1, "/.well-known/caldav"); // step 1: the well-known
    assert_eq!(seen[1].1, "/principals/users/dennis/"); // step 2: the principal
}

#[tokio::test]
async fn lists_only_calendar_collections() {
    let exec = Replay::new(vec![ok(include_str!(
        "../tests/fixtures/calendar-home.xml"
    ))]);
    let calendars = list_calendars(&exec, "/dav/cal/alice%40test.local/")
        .await
        .unwrap();
    // The home itself is a plain collection and is filtered out.
    assert_eq!(calendars.len(), 1);
    assert_eq!(
        calendars[0].id.as_str(),
        "/dav/cal/alice%40test.local/default/"
    );
}

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

#[tokio::test]
async fn a_response_without_a_home_set_is_an_error() {
    let exec = Replay::new(vec![ok(
        "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/x</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    )]);
    assert!(
        discover_home(&exec, "/x", &IgnoreConnectSteps)
            .await
            .is_err()
    );
}
