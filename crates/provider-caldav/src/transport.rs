//! The HTTP transport and the [`DavExecutor`] seam.
//!
//! Every CalDAV request the adapter makes goes through [`DavExecutor`], so the
//! discovery/sync orchestration is unit-tested offline by replaying captured
//! response bodies (mirroring `provider-jmap`'s `Executor`). The live
//! implementation, [`DavClient`], is a thin `reqwest` wrapper: it applies
//! authentication, sends the `PROPFIND`/`REPORT` method with a `Depth` header and
//! XML body, and — like the JMAP transport — **does not auto-follow redirects**,
//! so discovery can resolve the RFC 6764 well-known `307` itself.

use async_trait::async_trait;
use engine_provider::{HttpVersion, TlsVersion};

use crate::error::CalDavError;

/// How a host authenticates to the CalDAV server.
#[derive(Clone)]
#[non_exhaustive]
pub enum Credentials {
    /// HTTP Basic auth (RFC 7617) — the common CalDAV case.
    Basic {
        /// The user name.
        username: String,
        /// The password.
        password: String,
    },
    /// An OAuth 2.0 bearer token (RFC 6750), for providers that require it.
    Bearer(String),
}

impl core::fmt::Debug for Credentials {
    /// Redacts the secret: credentials must never reach logs (`north-star.md`).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Bearer(_) => f.debug_tuple("Bearer").field(&"<redacted>").finish(),
        }
    }
}

/// The WebDAV methods this adapter issues — the read reports plus the write verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DavMethod {
    /// `GET`, used for URI-backed CardDAV photos.
    Get,
    /// `OPTIONS` (RFC 4918 §10.1) — asks a resource which WebDAV compliance classes it
    /// supports, which is how RFC 6638 §2 says scheduling support is discovered.
    Options,
    /// `PROPFIND` (RFC 4918 §9.1).
    Propfind,
    /// `REPORT` (RFC 3253 §3.6; CalDAV/RFC 6578 reports).
    Report,
    /// `PUT` (RFC 4791 §5.3.2) — create or replace a calendar object resource.
    Put,
    /// `DELETE` (RFC 4918 §9.6) — remove a calendar object resource.
    Delete,
}

impl DavMethod {
    /// The HTTP method token.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Options => "OPTIONS",
            Self::Propfind => "PROPFIND",
            Self::Report => "REPORT",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// The conditional precondition guarding a write (RFC 7232; RFC 4791 §5.3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Precondition {
    /// `If-None-Match: *` — the resource must not already exist (a create).
    IfNoneMatch,
    /// `If-Match: <etag>` — the resource must still carry this entity tag (a
    /// guarded update or delete).
    IfMatch(String),
    /// `If-Schedule-Tag-Match: <tag>` preserves attendee replies while an organiser
    /// updates a scheduled object (RFC 6638 §3.2.10).
    IfScheduleTagMatch(String),
    /// No conditional header (an unconditional write).
    None,
}

/// A WebDAV write request (`PUT`/`DELETE`): the verb, target href, optional typed
/// body, and the conditional precondition. Distinct from the read [`DavExecutor::send`]
/// shape (Depth + XML), so the proven read path is untouched.
#[derive(Debug, Clone)]
pub(crate) struct WriteRequest {
    /// `PUT` or `DELETE`.
    pub method: DavMethod,
    /// The target resource href (absolute path or full URL).
    pub href: String,
    /// The `Content-Type` to send, when there is a body (`text/calendar` for a PUT).
    pub content_type: Option<&'static str>,
    /// The optimistic-concurrency precondition.
    pub precondition: Precondition,
    /// The request body (the iCalendar document for a PUT; empty for a DELETE).
    pub body: String,
}

// The reply type and its two readings live next door, and so does the live client; both are
// re-exported here because every caller reaches them through the transport.
pub(crate) use crate::{dav_client::DavClient, response::HttpResponse};

/// Executes one CalDAV request. Implemented by the live [`DavClient`] and, in
/// tests, by a fake replaying canned response documents.
#[async_trait]
pub(crate) trait DavExecutor: Send + Sync {
    /// Sends a **read** report — `method` to `href` (an absolute path or URL) with
    /// the `Depth` header and XML `body` — returning the raw response.
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError>;

    /// `href` as an absolute URL, resolved against the connection's base: the form a
    /// discovery step resolves the hrefs it reads against (RFC 4918 §8.3 resolves a relative
    /// href against the URL of the request that returned it). The default, for a fake with
    /// no origin, returns `href` unchanged.
    fn absolute(&self, href: &str) -> String {
        href.to_owned()
    }

    /// Lets `url`'s origin receive the credential once it asks for it: the account's own
    /// server led discovery there, by a redirect or by naming the principal or the home
    /// ([`crate::credential_scope`]).
    ///
    /// Returns `false` when the origin is **refused**: a plaintext origin on a connection
    /// that started on TLS, or a host the [`CredentialHosts`] policy does not permit. The
    /// caller fails discovery rather than going on without the credential. A relative `url`
    /// names no origin and is accepted. The default, for a fake, accepts everything.
    fn trust_origin(&self, _url: &str) -> bool {
        true
    }

    /// Whether a request to `href` carries the credential without being asked: its origin has
    /// been authenticated on this connection. The default, for a fake, says yes.
    fn sends_credential_to(&self, _href: &str) -> bool {
        true
    }

    /// Sends the credential to `href`'s origin from now on, because it answered naming the
    /// requester `<DAV:unauthenticated/>` (RFC 5397 §3) instead of challenging: a server that
    /// lets anonymous requests through. Returns whether that changed anything, so the caller asks
    /// again at most once; `false` for an origin that is not trusted. The default, for a fake,
    /// changes nothing.
    fn authenticate_unasked(&self, _href: &str) -> bool {
        false
    }

    /// Makes `home`'s origin the one later relative hrefs resolve against, and returns `home`
    /// in the form the provider keeps it: its path on that origin.
    ///
    /// Every href in a response from the home, and from the collections under it, is
    /// relative to the home's origin (RFC 4918 §8.3), so that is where the connection has to
    /// resolve them, whichever origin discovery started on. `home` must already be trusted
    /// ([`trust_origin`](Self::trust_origin)). The default, for a fake, returns `home`
    /// unchanged.
    fn settle_on(&self, home: &str) -> String {
        home.to_owned()
    }

    /// `OPTIONS` on `href`, so the response's `DAV` header can be read for the compliance
    /// classes the resource supports (RFC 4918 §10.1).
    ///
    /// The default routes through [`send`](DavExecutor::send) so a replay fake needs no
    /// extra plumbing. The live transport overrides it to send a **bare** `OPTIONS` — no
    /// `Depth`, no `Content-Type`, no body — because the read shape's XML framing is
    /// meaningless on a request that has nothing to say and only invites a stricter server
    /// to reject it.
    async fn send_options(&self, href: &str) -> Result<HttpResponse, CalDavError> {
        self.send(DavMethod::Options, href, "0", String::new())
            .await
    }

    /// Fetches an opaque binary resource without UTF-8 decoding.
    ///
    /// The default keeps replay fakes small. The live transport overrides it so
    /// image bytes never pass through [`HttpResponse::body`].
    async fn get_bytes(&self, href: &str) -> Result<Vec<u8>, CalDavError> {
        let response = self.send(DavMethod::Get, href, "0", String::new()).await?;
        if (200..300).contains(&response.status) {
            Ok(response.body.into_bytes())
        } else {
            Err(CalDavError::status(response.status, response.body)
                .with_retry_after(response.retry_after))
        }
    }

    /// Sends a **write** — a `PUT`/`DELETE` carrying a typed body and a conditional
    /// precondition instead of a `Depth` + XML body — returning the raw response
    /// (whose `ETag` header is the resource's new entity tag on a successful PUT).
    async fn send_write(&self, request: WriteRequest) -> Result<HttpResponse, CalDavError>;

    /// The HTTP version the transport negotiated. Defaults to `None`: only the live
    /// [`DavClient`] speaks HTTP, so a fake replaying canned documents has no version
    /// to report.
    fn http_version(&self) -> Option<HttpVersion> {
        None
    }

    /// The TLS version the transport negotiated, `None` for the same reason — and also
    /// for a live client talking to a plaintext `http://` origin, such as the harness.
    fn tls_version(&self) -> Option<TlsVersion> {
        None
    }
}

#[cfg(test)]
mod method_tests {
    use super::DavMethod;

    /// The tokens that go on the wire. Here rather than beside the response tests because
    /// `as_str` is this module's, and because a wrong token is a request failure, not a
    /// response one.
    #[test]
    fn dav_method_tokens() {
        assert_eq!(DavMethod::Propfind.as_str(), "PROPFIND");
        assert_eq!(DavMethod::Get.as_str(), "GET");
        assert_eq!(DavMethod::Report.as_str(), "REPORT");
        assert_eq!(DavMethod::Put.as_str(), "PUT");
        assert_eq!(DavMethod::Delete.as_str(), "DELETE");
    }
}
