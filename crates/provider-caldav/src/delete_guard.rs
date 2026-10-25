//! Holding a guarded `DELETE` to its `If-Match` where the server does not.
//!
//! Yahoo performs a `DELETE` whose `If-Match` names an ETag the resource no longer has, on its
//! calendar and its address-book server alike, while refusing the same guard on a `PUT`
//! (`docs/spec-deviations.md`, YAHOO-9; RFC 9110 §13.1.1). So before a guarded delete the
//! resource's current ETag is read, and a different one is answered with a `412` here, without
//! sending the delete. The read and the delete are two requests, so on such a server a change
//! landing between them still goes; the window shrinks from the age of the copy the delete was
//! made from to the time between two requests. A server that honours the guard still refuses
//! the delete itself.

use crate::{
    request::MEMBER_ETAGS_PROPFIND,
    transport::{DavExecutor, DavMethod, HttpResponse},
};

/// The `412` a delete of `href` guarded by `etag` would deserve, when the resource's current
/// ETag says so; `None` whenever it does not say so for certain, and the delete goes ahead
/// with its own guard.
pub(crate) async fn refused_before_delete(
    exec: &dyn DavExecutor,
    href: &str,
    etag: &str,
) -> Option<HttpResponse> {
    let current = exec
        .send(DavMethod::Propfind, href, "0", MEMBER_ETAGS_PROPFIND.into())
        .await
        .ok()?
        .into_multistatus()
        .ok()?
        .responses
        .iter()
        .find_map(|response| response.props.get("getetag").map(str::to_owned))?;
    (opaque(&current) != opaque(etag)).then(|| HttpResponse {
        status: 412,
        body: String::new(),
        location: None,
        etag: Some(current),
        dav: None,
        retry_after: None,
    })
}

/// An entity tag's opaque value, so a quoted `ETag` header and a `getetag` written without its
/// quotes, or as weak, compare equal: a false mismatch would refuse every delete.
fn opaque(etag: &str) -> &str {
    let etag = etag.trim();
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    etag.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(etag)
}
