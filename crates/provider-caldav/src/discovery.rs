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
//! Every href a step reads is resolved against the URL of the request that returned it
//! (RFC 4918 §8.3), so the principal and the home each keep the host that named them, and
//! each origin discovery is led to is trusted to receive the credential once it asks
//! ([`follow`]). `README.md` beside this crate draws the whole walk.
//!
//! The same `PROPFIND` asks for the principal's `calendar-user-address-set` (RFC 6638
//! §2.4.1), the addresses the server schedules as for this user. Only the principal carries
//! it, so the short-circuit leaves it unread and [`principal_addresses`] asks for it when a
//! host does.

use engine_core::{calendar::Calendar, version::CollectionTag};
use engine_provider::{CalendarUserAddresses, ConnectObserver, ConnectStep, IgnoreConnectSteps};

use crate::{
    calendar::{calendar_from_response, collection_tag},
    dav::MultiStatus,
    error::CalDavError,
    href::{redirect_href, resolve_reference},
    request::{CALENDAR_LIST_PROPFIND, CURRENT_USER_PRINCIPAL_PROPFIND, PRINCIPAL_PROPFIND},
    transport::{DavExecutor, DavMethod},
};

/// How many redirects discovery follows before giving up.
const MAX_REDIRECTS: usize = 4;

/// What discovery settled on: the calendar home, and what it learned about the user on the
/// way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CalendarHome {
    /// The calendar-home href, absolute when the transport has an origin.
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
    let start = trusted_start(exec, start_href)?;
    let (bootstrap, at) = propfind_principal(exec, &start, observer).await?;
    let named = current_user_principal(&bootstrap);
    if let Some(href) = home_set(&bootstrap) {
        // The home is already known, so the principal is only kept for asking it the address
        // set later; one the credential may not go to is dropped rather than failing the walk.
        let principal = named.and_then(|principal| follow(exec, &at, &principal).ok());
        // A resource that is not the principal answers the address set `404` (Stalwart's
        // does), which says nothing about the user; only a reported set is kept.
        return Ok(CalendarHome {
            href: follow(exec, &at, &href)?,
            principal,
            addresses: address_set(&bootstrap),
        });
    }
    // RFC 6764 §6: the calendar-home-set is a property of the principal resource,
    // so resolve the principal first, then ask it for the home-set.
    let principal = named
        .map(|principal| follow(exec, &at, &principal))
        .transpose()?;
    let principal = principal.ok_or_else(|| {
        CalDavError::protocol(
            "PROPFIND returned neither calendar-home-set nor current-user-principal",
        )
    })?;
    let (from_principal, at) = propfind_principal(exec, &principal, observer).await?;
    let href = home_set(&from_principal)
        .ok_or_else(|| CalDavError::protocol("principal PROPFIND returned no calendar-home-set"))?;
    Ok(CalendarHome {
        href: follow(exec, &at, &href)?,
        principal: Some(principal),
        addresses: Some(address_set(&from_principal).unwrap_or(CalendarUserAddresses::NotEnabled)),
    })
}

/// `start_href` as an absolute URL, its origin trusted to receive the credential once it asks.
///
/// The start is the host's configuration (its well-known URL, or a principal it stored), so it
/// is trusted under the same checks as any other origin.
///
/// # Errors
///
/// Returns [`CalDavError`] when the start names an origin the credential may not go to.
pub(crate) fn trusted_start(
    exec: &dyn DavExecutor,
    start_href: &str,
) -> Result<String, CalDavError> {
    let start = exec.absolute(start_href);
    if exec.trust_origin(&start) {
        Ok(start)
    } else {
        Err(CalDavError::protocol(format!(
            "the configured start {start:?} is a host the credential may not go to"
        )))
    }
}

/// Resolves `href`, which the response to a request at `at` named (a principal or a home),
/// and trusts its origin to receive the credential once it asks.
///
/// The account's own server named it, so it is where the account's data is, whichever host
/// that turns out to be: iCloud answers the principal on `caldav.icloud.com` and names the home
/// on `pNN-caldav.icloud.com`. The trust is the same one a redirect earns
/// ([`DavExecutor::trust_origin`]).
///
/// # Errors
///
/// Returns [`CalDavError`] when `href` cannot be resolved, or names an origin the credential
/// may not go to (a plaintext one after TLS, or a host the credential policy refuses): going
/// on without it would read the refusal of an unauthenticated request as the server's answer
/// about the account.
pub(crate) fn follow(exec: &dyn DavExecutor, at: &str, href: &str) -> Result<String, CalDavError> {
    let href = resolve_reference(at, href)?;
    if exec.trust_origin(&href) {
        Ok(href)
    } else {
        Err(CalDavError::protocol(format!(
            "the server named {href:?}, a host the credential may not go to"
        )))
    }
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
    let (response, _) = propfind_principal(exec, principal, &IgnoreConnectSteps).await?;
    Ok(address_set(&response).unwrap_or(CalendarUserAddresses::NotEnabled))
}

/// `PROPFIND`s `href` for the principal/home properties, following up to
/// [`MAX_REDIRECTS`] redirects and reporting each to `observer`. Returns the answer with the
/// href it came from, which the hrefs in it resolve against.
async fn propfind_principal(
    exec: &dyn DavExecutor,
    href: &str,
    observer: &dyn ConnectObserver,
) -> Result<(MultiStatus, String), CalDavError> {
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
                // The account's own server moved the chain, so its target may receive the
                // credential once it asks (`DavExecutor::trust_origin`), and is refused
                // outright when it would leave TLS or the credential policy refuses it.
                if !exec.trust_origin(&next) {
                    return Err(CalDavError::protocol(
                        "a discovery redirect left TLS or named a host the credential may not go to",
                    ));
                }
                href = next;
            }
            continue;
        }
        let multistatus = response.into_multistatus()?;
        // A server that lets an anonymous request through answers it naming the requester
        // `<DAV:unauthenticated/>` (RFC 5397 §3) rather than challenging: asked again once, with
        // the credential, when the origin may receive it.
        if multistatus.names_unauthenticated() && exec.authenticate_unasked(&href) {
            continue;
        }
        return Ok((multistatus, href));
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
    authenticate_before_reading(exec, home_href).await;
    Ok(exec
        .send_options(home_href)
        .await?
        .advertises(AUTO_SCHEDULE))
}

/// Makes sure `href`'s origin has been sent the credential before something is read there whose
/// answer depends on who asks, such as the DAV classes an `OPTIONS` reports: a server that lets
/// an anonymous `OPTIONS` through may answer it with less than the account has. A `PROPFIND`
/// for the requester draws the challenge, or names the requester unauthenticated (RFC 5397 §3),
/// and either way the credential follows. A failure here is left to the read that follows,
/// which meets the same server.
pub(crate) async fn authenticate_before_reading(exec: &dyn DavExecutor, href: &str) {
    if exec.sends_credential_to(href) {
        return;
    }
    let Ok(response) = exec
        .send(
            DavMethod::Propfind,
            href,
            "0",
            CURRENT_USER_PRINCIPAL_PROPFIND.to_owned(),
        )
        .await
    else {
        return;
    };
    if let Ok(multistatus) = response.into_multistatus()
        && multistatus.names_unauthenticated()
    {
        exec.authenticate_unasked(href);
    }
}

/// Lists the calendar collections under `home_href`, each with the tag the listing showed.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure or a malformed listing.
pub(crate) async fn list_calendars(
    exec: &dyn DavExecutor,
    home_href: &str,
) -> Result<Vec<(Calendar, Option<CollectionTag>)>, CalDavError> {
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
        // A collection that cannot hold events (a task list) is not a calendar here.
        .filter(|response| response.props.is_calendar() && response.props.holds_events())
        .map(|response| {
            Ok((
                calendar_from_response(response)?,
                collection_tag(&response.props),
            ))
        })
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

#[cfg(test)]
#[path = "home_origin_tests.rs"]
mod home_origin_tests;

#[cfg(test)]
#[path = "challenge_tests.rs"]
mod challenge_tests;
