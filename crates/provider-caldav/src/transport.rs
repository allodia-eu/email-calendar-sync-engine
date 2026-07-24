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
use engine_provider::{HttpVersion, ObservedHttpVersion};
use engine_tls::TlsClientConfig;
use reqwest::{Client, Method, redirect::Policy};

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
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
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

/// A WebDAV HTTP response reduced to what the adapter needs: the status, the body,
/// the `Location` header (so discovery can follow a well-known redirect), and the
/// `ETag` header (the new entity tag a successful `PUT` returns).
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
}

impl HttpResponse {
    /// Whether the status is a redirect carrying a new location (RFC 9110 — incl.
    /// `303 See Other`, which discovery must follow like the others).
    pub(crate) fn is_redirect(&self) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308) && self.location.is_some()
    }

    /// Returns the parsed [`MultiStatus`](crate::dav::MultiStatus) body, or a
    /// classified error for a non-`207` status.
    pub(crate) fn into_multistatus(self) -> Result<crate::dav::MultiStatus, CalDavError> {
        if self.status != 207 {
            return Err(CalDavError::status(self.status, self.body));
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
            Err(CalDavError::status(self.status, self.body))
        }
    }
}

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

    /// Fetches an opaque binary resource without UTF-8 decoding.
    ///
    /// The default keeps replay fakes small. The live transport overrides it so
    /// image bytes never pass through [`HttpResponse::body`].
    async fn get_bytes(&self, href: &str) -> Result<Vec<u8>, CalDavError> {
        let response = self.send(DavMethod::Get, href, "0", String::new()).await?;
        if (200..300).contains(&response.status) {
            Ok(response.body.into_bytes())
        } else {
            Err(CalDavError::status(response.status, response.body))
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
}

/// The live `reqwest`-backed CalDAV transport.
pub(crate) struct DavClient {
    client: Client,
    base: reqwest::Url,
    credentials: Credentials,
    /// The HTTP version most recently observed — the post-connect fact
    /// `ConnectionInfo::http_version` reports. Every response funnels through
    /// [`DavClient::collect`], and the discovery `PROPFIND` that
    /// [`CalDavProvider::connect`](crate::CalDavProvider::connect) performs populates it
    /// before a provider exists. It then keeps tracking: the RFC 6764 well-known `30x`
    /// this client follows *itself* may be a different origin from the calendar home
    /// that serves every real request, so the latest observation — not the first — is
    /// the one that describes the working connection.
    http_version: ObservedHttpVersion,
}

impl core::fmt::Debug for DavClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DavClient")
            .field("base", &self.base.as_str())
            .finish_non_exhaustive()
    }
}

impl DavClient {
    /// Builds a transport against `base_url` (the server origin), using the given
    /// credentials. Redirect-following is disabled so discovery handles the
    /// well-known `307` itself.
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] if `base_url` is not a valid URL or the HTTP client
    /// cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`); the
    /// library builds no trust store of its own.
    pub(crate) fn new(
        base_url: &str,
        credentials: Credentials,
        tls: &TlsClientConfig,
    ) -> Result<Self, CalDavError> {
        let base = reqwest::Url::parse(base_url)
            .map_err(|e| CalDavError::protocol(format!("bad base URL {base_url:?}: {e}")))?;
        let client = tls
            .reqwest_builder()
            .redirect(Policy::none())
            .build()
            .map_err(CalDavError::Transport)?;
        Ok(Self {
            client,
            base,
            credentials,
            http_version: ObservedHttpVersion::default(),
        })
    }
}

impl DavClient {
    /// Reduces a finished reqwest response to an [`HttpResponse`], reading its body and
    /// the `Location`/`ETag` headers — and recording the negotiated HTTP version on the
    /// way through. The one funnel every read and write response passes, so no path can
    /// forget to observe it.
    async fn collect(&self, response: reqwest::Response) -> Result<HttpResponse, CalDavError> {
        self.http_version.record(response.version());
        let status = response.status().as_u16();
        let header = |name: reqwest::header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let location = header(reqwest::header::LOCATION);
        let etag = header(reqwest::header::ETAG);
        let body = response.text().await?;
        Ok(HttpResponse {
            status,
            body,
            location,
            etag,
        })
    }

    /// Resolves `href` against the connection origin and builds a request for
    /// `method` — the shared head of every read and write.
    ///
    /// Credentials ride along **only when the resolved URL is same-origin with the
    /// account's base**. Most hrefs are server-issued and relative, so they resolve
    /// onto the base and authenticate as usual. But some come from remote *content* —
    /// a vCard `PHOTO;VALUE=uri` naming any host — and `Url::join` returns an absolute
    /// foreign URL unchanged. Authenticating those would send the account's Basic
    /// password (or bearer token) to whoever the card names, so they are fetched
    /// anonymously instead (`engine_provider::same_origin`).
    fn request(
        &self,
        method: DavMethod,
        href: &str,
    ) -> Result<reqwest::RequestBuilder, CalDavError> {
        let url = self
            .base
            .join(href)
            .map_err(|e| CalDavError::protocol(format!("bad href {href:?}: {e}")))?;
        let method = Method::from_bytes(method.as_str().as_bytes())
            .map_err(|e| CalDavError::protocol(format!("bad method: {e}")))?;
        let authenticate = engine_provider::same_origin(url.as_str(), self.base.as_str());
        let builder = self.client.request(method, url);
        if !authenticate {
            return Ok(builder);
        }
        Ok(match &self.credentials {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer(token) => builder.bearer_auth(token),
        })
    }
}

#[async_trait]
impl DavExecutor for DavClient {
    fn http_version(&self) -> Option<HttpVersion> {
        self.http_version.get()
    }

    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError> {
        let response = self
            .request(method, href)?
            .header("Depth", depth)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )
            .body(body)
            .send()
            .await?;
        self.collect(response).await
    }

    async fn get_bytes(&self, href: &str) -> Result<Vec<u8>, CalDavError> {
        let response = self.request(DavMethod::Get, href)?.send().await?;
        self.http_version.record(response.version());
        let status = response.status().as_u16();
        let bytes = response.bytes().await?;
        if (200..300).contains(&status) {
            Ok(bytes.to_vec())
        } else {
            Err(CalDavError::status(
                status,
                String::from_utf8_lossy(&bytes).into_owned(),
            ))
        }
    }

    async fn send_write(&self, request: WriteRequest) -> Result<HttpResponse, CalDavError> {
        let mut builder = self.request(request.method, &request.href)?;
        if let Some(content_type) = request.content_type {
            builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        builder = match request.precondition {
            // RFC 7232: `If-None-Match: *` admits only a create; `If-Match` admits
            // a replace/delete only while the entity tag is unchanged.
            Precondition::IfNoneMatch => builder.header(reqwest::header::IF_NONE_MATCH, "*"),
            Precondition::IfMatch(etag) => builder.header(reqwest::header::IF_MATCH, etag),
            Precondition::None => builder,
        };
        let response = builder.body(request.body).send().await?;
        self.collect(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, location: Option<&str>) -> HttpResponse {
        HttpResponse {
            status,
            body: String::new(),
            location: location.map(str::to_owned),
            etag: None,
        }
    }

    #[test]
    fn redirect_detection_requires_a_location() {
        assert!(response(307, Some("/dav/cal")).is_redirect());
        assert!(!response(307, None).is_redirect());
        // 303 See Other is a redirect too (must be followed by discovery).
        assert!(response(303, Some("/dav/cal")).is_redirect());
    }

    #[test]
    fn non_207_status_becomes_a_classified_error() {
        let unauthorized = HttpResponse {
            status: 401,
            body: "denied".to_owned(),
            location: None,
            etag: None,
        };
        let err = unauthorized.into_multistatus().unwrap_err();
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::Authentication
        );
    }

    #[test]
    fn dav_method_tokens() {
        assert_eq!(DavMethod::Propfind.as_str(), "PROPFIND");
        assert_eq!(DavMethod::Get.as_str(), "GET");
        assert_eq!(DavMethod::Report.as_str(), "REPORT");
        assert_eq!(DavMethod::Put.as_str(), "PUT");
        assert_eq!(DavMethod::Delete.as_str(), "DELETE");
    }

    #[test]
    fn write_success_yields_the_new_etag() {
        // A 2xx PUT returns the server's new entity tag (or None when it sent none).
        let created = HttpResponse {
            status: 201,
            body: String::new(),
            location: None,
            etag: Some("\"v9\"".to_owned()),
        };
        assert_eq!(
            created.into_write_etag().unwrap(),
            Some("\"v9\"".to_owned())
        );
        let no_content = HttpResponse {
            status: 204,
            body: String::new(),
            location: None,
            etag: None,
        };
        assert_eq!(no_content.into_write_etag().unwrap(), None);
    }

    #[test]
    fn write_precondition_failure_is_a_conflict() {
        // RFC 4791 §5.3.2: a failed If-Match/If-None-Match is 412 → Conflict, so
        // the caller refetches rather than blindly retrying.
        let precondition_failed = HttpResponse {
            status: 412,
            body: String::new(),
            location: None,
            etag: None,
        };
        let err = precondition_failed.into_write_etag().unwrap_err();
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::Conflict
        );
    }
}

// The live `DavClient` tests need a mock HTTP server; they live in a sibling file so
// this one stays under the line limit.
#[cfg(test)]
#[path = "transport_tests.rs"]
mod http_transport_tests;
