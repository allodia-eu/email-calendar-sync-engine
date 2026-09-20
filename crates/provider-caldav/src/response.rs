//! What one WebDAV reply is reduced to, and the two readings the adapter takes of it.
//!
//! Its own file for the 500-line limit, and the split falls where the responsibilities
//! already did: `transport.rs` gets a reply onto the wire and back, this decides what a
//! reply *means* — a `multistatus` to parse, a write's new `ETag`, or a classified
//! failure. `response_tests.rs` was already testing exactly this half.

use crate::error::CalDavError;

/// A WebDAV HTTP response reduced to what the adapter needs: the status, the body,
/// the `Location` header (so discovery can follow a well-known redirect), the
/// `ETag` header (the new entity tag a successful `PUT` returns), and the `DAV`
/// header (the compliance classes an `OPTIONS` reports).
#[derive(Debug, Clone)]
pub(crate) struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body (a `multistatus` document on success).
    pub body: String,
    /// The `Location` header, if the server sent a redirect.
    pub location: Option<String>,
    /// The `ETag` header, if the server returned one (a write's new entity tag).
    pub etag: Option<String>,
    /// The `DAV` header, if the server sent one (RFC 4918 §10.1): a comma-separated
    /// list of the compliance classes this resource supports.
    pub dav: Option<String>,
    /// When the server said to come back, on a throttle the shared funnel handed back
    /// rather than absorbing. `None` on everything else, and on the replay fakes — which
    /// is right: a fake has no server to have said anything.
    pub retry_after: Option<core::time::Duration>,
}

impl HttpResponse {
    /// Whether the response's `DAV` header advertises `token` as a compliance class
    /// (RFC 4918 §10.1).
    ///
    /// Tokens are comma-separated with optional whitespace, and a server chooses their
    /// case freely — Stalwart sends the header name lowercased over HTTP/2 while SabreDAV
    /// uppercases it — so the comparison is ASCII-case-insensitive on the trimmed token.
    /// Matching whole tokens rather than substrings matters: `calendar-access` is a prefix
    /// of nothing, but a substring search for it would also fire on a hypothetical
    /// `x-calendar-access`, and the classes this drives a capability off must not be
    /// guessed.
    pub(crate) fn advertises(&self, token: &str) -> bool {
        self.dav.as_deref().is_some_and(|header| {
            header
                .split(',')
                .any(|class| class.trim().eq_ignore_ascii_case(token))
        })
    }

    /// Whether the status is a redirect carrying a new location (RFC 9110 — incl.
    /// `303 See Other`, which discovery must follow like the others).
    pub(crate) fn is_redirect(&self) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308) && self.location.is_some()
    }

    /// Returns the parsed [`MultiStatus`](crate::dav::MultiStatus) body, or a
    /// classified error for a non-`207` status.
    pub(crate) fn into_multistatus(self) -> Result<crate::dav::MultiStatus, CalDavError> {
        if self.status != 207 {
            return Err(
                CalDavError::status(self.status, self.body).with_retry_after(self.retry_after)
            );
        }
        crate::dav::parse_multistatus(&self.body)
    }

    /// For a write (`PUT`/`DELETE`): the new `ETag` (if the server sent one) on a
    /// `2xx`, or a classified error otherwise — `412` becomes a
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict) so a
    /// precondition failure is refetched, not blindly retried (`error.rs`).
    pub(crate) fn into_write_etag(self) -> Result<Option<String>, CalDavError> {
        if (200..300).contains(&self.status) {
            Ok(self.etag)
        } else {
            Err(CalDavError::status(self.status, self.body).with_retry_after(self.retry_after))
        }
    }
}

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;
