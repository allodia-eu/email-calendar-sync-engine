//! Finding the calendar homes of other principals that the credential may read: a CalDAV
//! store shared with the user (`engine_provider::shared`).
//!
//! CalDAV names no other principal's home in the user's own `calendar-home-set`. Two routes
//! lead to one, both starting at the user's principal ([`DELEGATION_PROPFIND`]):
//!
//! 1. the CalendarServer proxy sets, `calendar-proxy-read-for` and `calendar-proxy-write-for`,
//!    which name the principals that made the user their calendar proxy (SabreDAV and Nextcloud
//!    answer them; Stalwart answers `404`);
//! 2. RFC 3744 §5.8's `principal-collection-set`, a collection a Depth-1 `PROPFIND` lists the
//!    principals of (Stalwart and SabreDAV list it; Nextcloud refuses the listing `405`).
//!
//! Neither says which homes the credential may read. Stalwart's principal collection lists
//! every principal the user holds *any* share from (bob, for his INBOX), and bob's home then
//! answers `403`; SabreDAV's answers bob's home `207` with every property `403` inside. So a
//! principal is offered only when its home lists at least one calendar the credential is
//! granted properties on (`provider-caldav/tests/live_sabredav_delegation.rs`,
//! `stalwart-harness/tests/smoke_shared_stores.rs`).
//!
//! The handle is the principal's URL; binding starts discovery there
//! (`discovery::discover_shared_home`). Every principal and home followed here goes through
//! the same credential-scope check as discovery ([`follow`]), so the credential never reaches
//! an origin the user did not approve.

use std::collections::BTreeMap;

use engine_core::{
    ids::{ProviderKey, SharedAccountId},
    sync::SearchDomain,
};
use engine_provider::{CalendarUserAddresses, SharedAccount, StoreDomains};

use crate::{
    dav::{MultiStatus, Props},
    discovery::follow,
    error::CalDavError,
    request::{CALENDAR_LIST_PROPFIND, DELEGATION_PROPFIND, PRINCIPAL_DESCRIPTION_PROPFIND},
    transport::{DavExecutor, DavMethod},
};

/// The stores whose calendars the credential may read, found from `user_principal`.
///
/// # Errors
///
/// A transport failure, or a server error other than a refusal (`401`, `403`, `404`, `405`), which
/// says nothing about whether a store exists and so fails the listing rather than shortening
/// it.
pub(crate) async fn list(
    exec: &dyn DavExecutor,
    user_principal: &str,
) -> Result<Vec<SharedAccount>, CalDavError> {
    let Some(routes) = propfind(exec, user_principal, "0", DELEGATION_PROPFIND).await? else {
        return Ok(Vec::new());
    };
    let props = routes
        .responses
        .first()
        .map(|response| response.props.clone())
        .unwrap_or_default();

    // Each candidate principal, by URL, with its description once one is read.
    let mut candidates: BTreeMap<String, Option<Props>> = BTreeMap::new();
    let mut order = Vec::new();
    let mut add = |url: String, described: Option<Props>| {
        if same_resource(&url, user_principal) {
            return;
        }
        let entry = candidates.entry(url.clone()).or_insert_with(|| {
            order.push(url);
            None
        });
        if entry.is_none() {
            *entry = described;
        }
    };
    for set in ["calendar-proxy-read-for", "calendar-proxy-write-for"] {
        for href in props.hrefs(set).unwrap_or_default() {
            if let Ok(url) = follow(exec, user_principal, href) {
                add(url, None);
            }
        }
    }
    for href in props.hrefs("principal-collection-set").unwrap_or_default() {
        let Ok(collection) = follow(exec, user_principal, href) else {
            continue;
        };
        let Some(listing) =
            propfind(exec, &collection, "1", PRINCIPAL_DESCRIPTION_PROPFIND).await?
        else {
            continue;
        };
        for response in &listing.responses {
            if let Ok(url) = follow(exec, &collection, response.href())
                && !same_resource(&url, &collection)
            {
                add(url, Some(response.props.clone()));
            }
        }
    }

    let mut stores = Vec::new();
    for principal in order {
        let described = match candidates.remove(&principal).flatten() {
            Some(props) => Some(props),
            None => describe(exec, &principal).await?,
        };
        let Some(props) = described else {
            continue;
        };
        let Some(home) = props
            .get("calendar-home-set")
            .and_then(|home| follow(exec, &principal, home).ok())
        else {
            continue;
        };
        if reads_a_calendar(exec, &home).await? {
            stores.push(project(&principal, &props)?);
        }
    }
    Ok(stores)
}

/// The first address in a principal's `calendar-user-address-set`, if it names one.
pub(crate) fn principal_address(props: &Props) -> Option<String> {
    let set = CalendarUserAddresses::from_uris(
        props
            .hrefs("calendar-user-address-set")
            .unwrap_or_default()
            .iter()
            .map(String::as_str),
    );
    set.addresses().first().cloned()
}

/// Reads one principal's description, or `None` when the credential may not.
pub(crate) async fn describe(
    exec: &dyn DavExecutor,
    principal: &str,
) -> Result<Option<Props>, CalDavError> {
    Ok(
        propfind(exec, principal, "0", PRINCIPAL_DESCRIPTION_PROPFIND)
            .await?
            .and_then(|answer| answer.responses.into_iter().next())
            .map(|response| response.props),
    )
}

/// Whether `home` lists a calendar collection the credential is granted properties on.
async fn reads_a_calendar(exec: &dyn DavExecutor, home: &str) -> Result<bool, CalDavError> {
    Ok(propfind(exec, home, "1", CALENDAR_LIST_PROPFIND)
        .await?
        .is_some_and(|listing| {
            listing
                .responses
                .iter()
                .any(|response| response.props.is_calendar() && response.props.holds_events())
        }))
}

/// A `PROPFIND`, with a refusal read as "not for this credential": `401`, `403` or `404`, and
/// `405`, which is how Nextcloud refuses to list a principal collection ("Listing members of
/// this collection is disabled"), leaving the proxy sets as its only route.
async fn propfind(
    exec: &dyn DavExecutor,
    href: &str,
    depth: &str,
    body: &str,
) -> Result<Option<MultiStatus>, CalDavError> {
    let response = exec
        .send(DavMethod::Propfind, href, depth, body.to_owned())
        .await?;
    if matches!(response.status, 401 | 403 | 404 | 405) {
        return Ok(None);
    }
    response.into_multistatus().map(Some)
}

fn project(principal: &str, props: &Props) -> Result<SharedAccount, CalDavError> {
    let handle = SharedAccountId::new(
        ProviderKey::new(principal.to_owned())
            .map_err(|e| CalDavError::protocol(format!("bad principal URL {principal:?}: {e}")))?,
    );
    let address = principal_address(props);
    let name = props
        .get("displayname")
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| address.clone())
        .unwrap_or_else(|| principal.to_owned());
    let store = SharedAccount::new(handle, name, StoreDomains::of(SearchDomain::Calendar));
    Ok(match address {
        Some(address) => store.with_address(address),
        None => store,
    })
}

/// Whether two absolute URLs name the same resource: a trailing slash and the encoding of
/// `@` (Stalwart writes `%40`, SabreDAV a bare `@`) do not make a different principal.
pub(crate) fn same_resource(a: &str, b: &str) -> bool {
    let canonical = |url: &str| url.trim_end_matches('/').replace("%40", "@");
    canonical(a) == canonical(b)
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
