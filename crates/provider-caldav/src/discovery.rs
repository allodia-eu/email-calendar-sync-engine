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

use crate::calendar::calendar_from_response;
use crate::dav::MultiStatus;
use crate::error::CalDavError;
use crate::request::{CALENDAR_LIST_PROPFIND, PRINCIPAL_PROPFIND};
use crate::transport::{DavExecutor, DavMethod};

/// How many redirects discovery follows before giving up.
const MAX_REDIRECTS: usize = 4;

/// Resolves the calendar-home href, starting at `start_href`.
///
/// `PROPFIND`s the start URL; if it returns the `calendar-home-set` directly
/// (lenient servers), uses it, otherwise follows the RFC 6764 §6 second step and
/// `PROPFIND`s the returned `current-user-principal` for its home-set. Each
/// `PROPFIND` follows up to [`MAX_REDIRECTS`] redirects.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure, a redirect loop, or a
/// response with neither a `calendar-home-set` nor a `current-user-principal`.
pub(crate) async fn discover_home(
    exec: &dyn DavExecutor,
    start_href: &str,
) -> Result<String, CalDavError> {
    let bootstrap = propfind_principal(exec, start_href).await?;
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
    let from_principal = propfind_principal(exec, &principal).await?;
    home_set(&from_principal)
        .ok_or_else(|| CalDavError::protocol("principal PROPFIND returned no calendar-home-set"))
}

/// `PROPFIND`s `href` for the principal/home properties, following up to
/// [`MAX_REDIRECTS`] redirects.
async fn propfind_principal(
    exec: &dyn DavExecutor,
    href: &str,
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
            href = response.location.clone().unwrap_or(href);
            continue;
        }
        return response.into_multistatus();
    }
    Err(CalDavError::protocol(
        "too many redirects resolving the calendar home",
    ))
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
    use super::*;
    use crate::test_support::{Replay, ok};
    use crate::transport::HttpResponse;

    #[tokio::test]
    async fn follows_a_redirect_then_reads_the_home() {
        let redirect = HttpResponse {
            status: 307,
            body: String::new(),
            location: Some("/dav/cal".to_owned()),
        };
        let exec = Replay::new(vec![
            redirect,
            ok(include_str!("../tests/fixtures/principal.xml")),
        ]);
        let home = discover_home(&exec, "/.well-known/caldav").await.unwrap();
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

        let home = discover_home(&exec, "/.well-known/caldav").await.unwrap();
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
    async fn a_response_without_a_home_set_is_an_error() {
        let exec = Replay::new(vec![ok(
            "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/x</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
        )]);
        assert!(discover_home(&exec, "/x").await.is_err());
    }
}
