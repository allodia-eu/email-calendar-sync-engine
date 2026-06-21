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
use reqwest::redirect::Policy;
use reqwest::{Client, Method};

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

/// The WebDAV methods this read adapter issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DavMethod {
    /// `PROPFIND` (RFC 4918 §9.1).
    Propfind,
    /// `REPORT` (RFC 3253 §3.6; CalDAV/RFC 6578 reports).
    Report,
}

impl DavMethod {
    /// The HTTP method token.
    fn as_str(self) -> &'static str {
        match self {
            Self::Propfind => "PROPFIND",
            Self::Report => "REPORT",
        }
    }
}

/// A WebDAV HTTP response reduced to what the adapter needs: the status, the body,
/// and the `Location` header (so discovery can follow a well-known redirect).
#[derive(Debug, Clone)]
pub(crate) struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body (a `multistatus` document on success).
    pub body: String,
    /// The `Location` header, if the server sent a redirect.
    pub location: Option<String>,
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
}

/// Executes one CalDAV request. Implemented by the live [`DavClient`] and, in
/// tests, by a fake replaying canned response documents.
#[async_trait]
pub(crate) trait DavExecutor: Send + Sync {
    /// Sends `method` to `href` (an absolute path or URL) with the `Depth` header
    /// and XML `body`, returning the raw response.
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError>;
}

/// The live `reqwest`-backed CalDAV transport.
pub(crate) struct DavClient {
    client: Client,
    base: reqwest::Url,
    credentials: Credentials,
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
    pub(crate) fn new(base_url: &str, credentials: Credentials) -> Result<Self, CalDavError> {
        let base = reqwest::Url::parse(base_url)
            .map_err(|e| CalDavError::protocol(format!("bad base URL {base_url:?}: {e}")))?;
        let client = Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(CalDavError::Transport)?;
        Ok(Self {
            client,
            base,
            credentials,
        })
    }
}

#[async_trait]
impl DavExecutor for DavClient {
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError> {
        let url = self
            .base
            .join(href)
            .map_err(|e| CalDavError::protocol(format!("bad href {href:?}: {e}")))?;
        let method = Method::from_bytes(method.as_str().as_bytes())
            .map_err(|e| CalDavError::protocol(format!("bad method: {e}")))?;
        let mut builder = self
            .client
            .request(method, url)
            .header("Depth", depth)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )
            .body(body);
        builder = match &self.credentials {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer(token) => builder.bearer_auth(token),
        };
        let response = builder.send().await?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await?;
        Ok(HttpResponse {
            status,
            body,
            location,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_detection_requires_a_location() {
        let with_location = HttpResponse {
            status: 307,
            body: String::new(),
            location: Some("/dav/cal".to_owned()),
        };
        assert!(with_location.is_redirect());
        let no_location = HttpResponse {
            status: 307,
            body: String::new(),
            location: None,
        };
        assert!(!no_location.is_redirect());
        // 303 See Other is a redirect too (must be followed by discovery).
        let see_other = HttpResponse {
            status: 303,
            body: String::new(),
            location: Some("/dav/cal".to_owned()),
        };
        assert!(see_other.is_redirect());
    }

    #[test]
    fn non_207_status_becomes_a_classified_error() {
        let unauthorized = HttpResponse {
            status: 401,
            body: "denied".to_owned(),
            location: None,
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
        assert_eq!(DavMethod::Report.as_str(), "REPORT");
    }
}
