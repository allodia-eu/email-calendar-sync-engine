//! CalDAV discovery: principal → calendar-home → calendar collections
//! (RFC 6764 §6, RFC 4791 §6.2.1).
//!
//! Discovery is the **two-step** RFC 6764 flow: `PROPFIND` the starting URL (the
//! well-known path by default) for the `current-user-principal`, then `PROPFIND`
//! that **principal** resource for its `calendar-home-set` — the home-set is a
//! property of the principal, not of the root. A lenient server (Stalwart) returns
//! the home-set directly at the start URL, so that short-circuits the second step.
//! Either `PROPFIND` follows redirects itself (the transport does not auto-follow,
//! mirroring the JMAP session flow). Discovery then lists the home's collections at
//! `Depth: 1`, keeping those whose `resourcetype` marks them a calendar.

use engine_core::calendar::Calendar;
use engine_provider::{ConnectObserver, ConnectStep};

use crate::{
    calendar::calendar_from_response,
    dav::MultiStatus,
    error::CalDavError,
    request::{CALENDAR_LIST_PROPFIND, PRINCIPAL_PROPFIND},
    transport::{DavExecutor, DavMethod},
};

/// How many redirects discovery follows before giving up.
const MAX_REDIRECTS: usize = 4;

/// Resolves the calendar-home href, starting at `start_href`.
///
/// `PROPFIND`s the start URL; if it returns the `calendar-home-set` directly
/// (lenient servers), uses it, otherwise follows the RFC 6764 §6 second step and
/// `PROPFIND`s the returned `current-user-principal` for its home-set. Each
/// `PROPFIND` follows up to [`MAX_REDIRECTS`] redirects, reporting one
/// [`ConnectStep::Redirected`] per hop to `observer`.
///
/// The principal → home-set step is **not** a redirect and emits nothing: it is a
/// second `PROPFIND` of a different resource, not the same resource moving.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure, a redirect loop, or a
/// response with neither a `calendar-home-set` nor a `current-user-principal`.
pub(crate) async fn discover_home(
    exec: &dyn DavExecutor,
    start_href: &str,
    observer: &dyn ConnectObserver,
) -> Result<String, CalDavError> {
    let bootstrap = propfind_principal(exec, start_href, observer).await?;
    if let Some(home) = home_set(&bootstrap) {
        return Ok(home);
    }
    // RFC 6764 §6: the calendar-home-set is a property of the principal resource,
    // so resolve the principal first, then ask it for the home-set.
    let principal = current_user_principal(&bootstrap).ok_or_else(|| {
        CalDavError::protocol(
            "PROPFIND returned neither calendar-home-set nor current-user-principal",
        )
    })?;
    let from_principal = propfind_principal(exec, &principal, observer).await?;
    home_set(&from_principal)
        .ok_or_else(|| CalDavError::protocol("principal PROPFIND returned no calendar-home-set"))
}

/// `PROPFIND`s `href` for the principal/home properties, following up to
/// [`MAX_REDIRECTS`] redirects and reporting each to `observer`.
async fn propfind_principal(
    exec: &dyn DavExecutor,
    href: &str,
    observer: &dyn ConnectObserver,
) -> Result<MultiStatus, CalDavError> {
    let mut href = href.to_owned();
    for _ in 0..MAX_REDIRECTS {
        let response = exec
            .send(
                DavMethod::Propfind,
                &href,
                "0",
                PRINCIPAL_PROPFIND.to_owned(),
            )
            .await?;
        if response.is_redirect() {
            // `is_redirect()` is true only with a `Location`, so this always binds;
            // without one the loop re-requests `href` and exhausts `MAX_REDIRECTS`,
            // exactly as before.
            if let Some(location) = &response.location {
                observer.step(&ConnectStep::redirected(&href, location));
                href.clone_from(location);
            }
            continue;
        }
        return response.into_multistatus();
    }
    Err(CalDavError::protocol(
        "too many redirects resolving the calendar home",
    ))
}

/// The RFC 6638 §2 compliance class a server advertises when it schedules for itself.
const AUTO_SCHEDULE: &str = "calendar-auto-schedule";

/// Asks `home_href` whether this server performs RFC 6638 scheduling.
///
/// RFC 4791 is calendar **access**; scheduling is a separate specification layered on top,
/// and RFC 6638 §2 says a conforming server advertises `calendar-auto-schedule` in the
/// `DAV:` header of an `OPTIONS` response. Without asking, a plain CalDAV server looks
/// identical to an auto-scheduling one right up until an RSVP is stored and the organizer
/// is never told — so this is discovered at connect, not assumed
/// ([`Capabilities::calendar_scheduling`](engine_provider::Capabilities::calendar_scheduling)).
///
/// The target is the **calendar home**, not the connection's base URL: the header belongs to
/// a DAV resource, and a server's site root need not be one — Stalwart's answers `302` to its
/// web UI with no `DAV:` header at all.
///
/// A response that carries no such token — whatever its status — means "not advertised",
/// which is a `false` capability and not an error: a server may answer `OPTIONS` with a
/// `405`, and a connect that failed over it would refuse an account that reads and writes
/// perfectly well. A transport failure still propagates, like every other discovery step.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport failure.
pub(crate) async fn discover_scheduling(
    exec: &dyn DavExecutor,
    home_href: &str,
) -> Result<bool, CalDavError> {
    Ok(exec
        .send_options(home_href)
        .await?
        .advertises(AUTO_SCHEDULE))
}

/// Lists the calendar collections under `home_href`.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure or a malformed listing.
pub(crate) async fn list_calendars(
    exec: &dyn DavExecutor,
    home_href: &str,
) -> Result<Vec<Calendar>, CalDavError> {
    let listing = exec
        .send(
            DavMethod::Propfind,
            home_href,
            "1",
            CALENDAR_LIST_PROPFIND.to_owned(),
        )
        .await?
        .into_multistatus()?;
    listing
        .responses
        .iter()
        .filter(|response| response.props.is_calendar())
        .map(calendar_from_response)
        .collect()
}

/// Reads the first `calendar-home-set` href from a discovery response.
fn home_set(multistatus: &MultiStatus) -> Option<String> {
    multistatus
        .responses
        .iter()
        .find_map(|response| response.props.get("calendar-home-set").map(str::to_owned))
}

/// Reads the first `current-user-principal` href from a discovery response.
fn current_user_principal(multistatus: &MultiStatus) -> Option<String> {
    multistatus.responses.iter().find_map(|response| {
        response
            .props
            .get("current-user-principal")
            .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
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
        };
        let exec = Replay::new(vec![
            redirect,
            ok(include_str!("../tests/fixtures/principal.xml")),
        ]);
        let home = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
            .await
            .unwrap();
        assert_eq!(home, "/dav/cal/alice%40test.local/");
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
        assert_eq!(home, "/calendars/dennis/");
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
}
