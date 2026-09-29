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
//!
//! The same `PROPFIND` asks for the principal's `calendar-user-address-set` (RFC 6638
//! §2.4.1), the addresses the server schedules as for this user. Only the principal carries
//! it, so the short-circuit leaves it unread and [`principal_addresses`] asks for it when a
//! host does.

use engine_core::calendar::Calendar;
use engine_provider::{CalendarUserAddresses, ConnectObserver, ConnectStep, IgnoreConnectSteps};

use crate::{
    calendar::calendar_from_response,
    dav::MultiStatus,
    error::CalDavError,
    href::redirect_href,
    request::{CALENDAR_LIST_PROPFIND, PRINCIPAL_PROPFIND},
    transport::{DavExecutor, DavMethod},
};

/// How many redirects discovery follows before giving up.
const MAX_REDIRECTS: usize = 4;

/// What discovery settled on: the calendar home, and what it learned about the user on the
/// way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CalendarHome {
    /// The calendar-home href.
    pub(crate) href: String,
    /// The `current-user-principal`, when the server named one.
    pub(crate) principal: Option<String>,
    /// The principal's address set, when discovery read the principal itself; `None` when
    /// the home was found without asking it.
    pub(crate) addresses: Option<CalendarUserAddresses>,
}

/// Resolves the calendar home, starting at `start_href`.
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
) -> Result<CalendarHome, CalDavError> {
    let bootstrap = propfind_principal(exec, start_href, observer).await?;
    let principal = current_user_principal(&bootstrap);
    if let Some(href) = home_set(&bootstrap) {
        // A resource that is not the principal answers the address set `404` (Stalwart's
        // does), which says nothing about the user; only a reported set is kept.
        return Ok(CalendarHome {
            href,
            principal,
            addresses: address_set(&bootstrap),
        });
    }
    // RFC 6764 §6: the calendar-home-set is a property of the principal resource,
    // so resolve the principal first, then ask it for the home-set.
    let principal = principal.ok_or_else(|| {
        CalDavError::protocol(
            "PROPFIND returned neither calendar-home-set nor current-user-principal",
        )
    })?;
    let from_principal = propfind_principal(exec, &principal, observer).await?;
    let href = home_set(&from_principal)
        .ok_or_else(|| CalDavError::protocol("principal PROPFIND returned no calendar-home-set"))?;
    Ok(CalendarHome {
        href,
        principal: Some(principal),
        addresses: Some(address_set(&from_principal).unwrap_or(CalendarUserAddresses::NotEnabled)),
    })
}

/// Asks `principal` for the addresses the server schedules as for this user.
///
/// The principal is the resource RFC 6638 §2.4.1 puts the property on, so a principal that
/// does not report it is [`CalendarUserAddresses::NotEnabled`] rather than unknown.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure or a redirect loop.
pub(crate) async fn principal_addresses(
    exec: &dyn DavExecutor,
    principal: &str,
) -> Result<CalendarUserAddresses, CalDavError> {
    let response = propfind_principal(exec, principal, &IgnoreConnectSteps).await?;
    Ok(address_set(&response).unwrap_or(CalendarUserAddresses::NotEnabled))
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
            // without one the loop re-requests `href` and exhausts [`MAX_REDIRECTS`],
            // exactly as before.
            if let Some(location) = &response.location {
                let next = redirect_href(&href, location).ok_or_else(|| {
                    CalDavError::protocol(format!("unresolvable redirect to {location:?}"))
                })?;
                observer.step(&ConnectStep::redirected(&href, &next));
                // The account's own server moved the chain, so the connection follows it:
                // credentials travel to the new origin and later relative hrefs resolve
                // there. A no-op until a hop names an origin (`DavExecutor::adopt_origin`),
                // and refused outright when that origin would leave TLS.
                if !exec.adopt_origin(&next) {
                    return Err(CalDavError::protocol(
                        "a discovery redirect left TLS; refusing to send the credential in the clear",
                    ));
                }
                href = next;
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

/// Reads the first reported `calendar-user-address-set` from a discovery response, keeping
/// its `mailto:` entries.
fn address_set(multistatus: &MultiStatus) -> Option<CalendarUserAddresses> {
    multistatus.responses.iter().find_map(|response| {
        let hrefs = response.props.hrefs("calendar-user-address-set")?;
        Some(CalendarUserAddresses::from_uris(
            hrefs.iter().map(String::as_str),
        ))
    })
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
#[path = "discovery_tests.rs"]
mod tests;
