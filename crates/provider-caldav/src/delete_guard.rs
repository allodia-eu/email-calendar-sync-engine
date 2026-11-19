//! Holding a guarded `DELETE` to its `If-Match` where the server does not.
//!
//! Yahoo performs a `DELETE` whose `If-Match` names an ETag the resource no longer has, on its
//! calendar and its address-book server alike, while refusing the same guard on a `PUT`
//! (`docs/spec-deviations.md`, YAHOO-9; RFC 9110 §13.1.1). So before a guarded delete the
//! resource's current tag is read, and a different one is answered with a `412` here, without
//! sending the delete. The read and the delete are two requests, so on such a server a change
//! landing between them still goes; the window shrinks from the age of the copy the delete was
//! made from to the time between two requests. A server that honours the guard still refuses
//! the delete itself.

use crate::{
    request::{MEMBER_ETAGS_PROPFIND, SCHEDULE_TAG_PROPFIND},
    transport::{DavExecutor, DavMethod, HttpResponse, Precondition},
};

/// The `412` a delete of `href` under `guard` would deserve, when the resource's current tag
/// says so; `None` whenever it does not say so for certain, and the delete goes ahead with its
/// own guard. An `If-Match` is checked against the resource's `getetag`, and an
/// `If-Schedule-Tag-Match` (RFC 6638 §3.2.10) against its `schedule-tag`.
pub(crate) async fn refused_before_delete(
    exec: &dyn DavExecutor,
    href: &str,
    guard: &Precondition,
) -> Option<HttpResponse> {
    let (body, property, expected) = match guard {
        Precondition::IfMatch(etag) => (MEMBER_ETAGS_PROPFIND, "getetag", etag),
        Precondition::IfScheduleTagMatch(tag) => (SCHEDULE_TAG_PROPFIND, "schedule-tag", tag),
        Precondition::IfNoneMatch | Precondition::None => return None,
    };
    let current = exec
        .send(DavMethod::Propfind, href, "0", body.into())
        .await
        .ok()?
        .into_multistatus()
        .ok()?
        .responses
        .iter()
        .find_map(|response| response.props.get(property).map(str::to_owned))?;
    (opaque(&current) != opaque(expected)).then(|| HttpResponse {
        status: 412,
        body: String::new(),
        location: None,
        etag: None,
        dav: None,
        retry_after: None,
    })
}

/// An entity tag's opaque value, so a quoted `ETag` header and a `getetag` written without its
/// quotes, or as weak, compare equal: a false mismatch would refuse every delete.
fn opaque(etag: &str) -> &str {
    let etag = etag.trim();
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    etag.strip_circumfix('"', '"').unwrap_or(etag)
}
