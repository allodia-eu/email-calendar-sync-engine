//! The Google transport seam ([`GoogleTransport`]) and connected client
//! ([`GoogleClient`]).
//!
//! Google has no session-discovery step (like Graph, unlike JMAP): the API root is
//! fixed and the adapter builds relative paths (`/gmail/v1/…`, `/calendar/v3/…`) and
//! `GET`s/`POST`s them. Pagination and delta cursors are **opaque tokens** Google
//! returns (`nextPageToken`, `nextSyncToken`, Gmail's `historyId`), which the fetch
//! layer threads back as query parameters it builds itself — so, unlike Graph's
//! absolute `@odata` links, there is **no URL to rebase**, and a `with_base` replay
//! server is reached simply because the client roots every path at that base.
//!
//! A non-2xx response becomes a classified [`GoogleError::Status`] with the Google
//! error `reason` extracted from the body.
//!
//! The [`GoogleTransport`] seam lets the fetch/provider orchestration be unit-tested
//! offline against captured fixtures; the production reqwest implementation
//! ([`HttpTransport`](crate::http_transport)) lives in `http_transport`.

use async_trait::async_trait;
use engine_provider::HttpVersion;
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{error::GoogleError, http_transport::HttpTransport};

/// The universal Google APIs host — serves both `gmail/v1/…` and `calendar/v3/…`.
pub(crate) const GOOGLE_BASE: &str = "https://www.googleapis.com";

/// An authenticated request against a Google API.
///
/// Implemented by [`HttpTransport`](crate::http_transport) (live reqwest) and, in
/// tests, by a fake fed canned fixtures keyed by URL — so the whole fetch
/// orchestration runs offline.
#[async_trait]
pub(crate) trait GoogleTransport: Send + Sync {
    /// Fetches `url`, returning the parsed JSON or a classified error.
    async fn get(&self, url: &str) -> Result<Value, GoogleError>;

    /// Fetches authenticated raw bytes from a Google API URL.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GoogleError>;

    /// Fetches raw bytes **without** the account's OAuth token, for a URL that came
    /// from remote content rather than from the API root — a People `photos[].url`
    /// points at `googleusercontent.com`, which serves it publicly. Sending the token
    /// off-origin would hand it to whatever host the payload names.
    async fn get_bytes_unauthenticated(&self, url: &str) -> Result<Vec<u8>, GoogleError>;

    /// `POST`s `body` with `content_type` to `url`, returning the parsed JSON response
    /// body when the server sent one — an action answering with an empty body yields
    /// `None`. A non-2xx becomes a classified [`GoogleError::Status`]. Gmail's
    /// `messages.modify`/`send`/`trash` and Calendar's `events.insert` post here.
    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError>;

    /// `PATCH`es `body` with `content_type` to `url`, guarded by `if_match` (an `If-Match`
    /// ETag precondition; a stale one is `412` → [`FailureClass::Conflict`]). Returns the
    /// updated object's JSON (Google echoes it). Calendar's `events.patch` posts here.
    ///
    /// [`FailureClass::Conflict`]: engine_core::error::FailureClass::Conflict
    async fn patch(
        &self,
        url: &str,
        content_type: &str,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError>;

    /// `DELETE`s `url`, guarded by `if_match` (used by Calendar's `events.delete`; Gmail's
    /// `messages.delete` passes `None`). A `2xx` (Google answers `204`) is success; a
    /// non-2xx becomes a classified [`GoogleError::Status`] (a `404` — already gone — is
    /// the caller's to treat as idempotent success).
    async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GoogleError>;

    /// The HTTP version the transport negotiated, or `None` before its first response.
    /// Defaults to `None`: only the reqwest transport speaks HTTP, so a fake fed canned
    /// fixtures has no version to report.
    fn http_version(&self) -> Option<HttpVersion> {
        None
    }
}

/// A connected Google client: an authenticated transport plus the API root.
///
/// Built with [`GoogleClient::connect`] (an OAuth bearer access token; the engine
/// stays OAuth-agnostic, so token acquisition/refresh is the host's job —
/// `north-star.md`). The fetch layer builds API-relative paths and issues them
/// through the crate-internal `url`/`get`/… methods.
pub struct GoogleClient {
    transport: Box<dyn GoogleTransport>,
    base: String,
}

impl core::fmt::Debug for GoogleClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GoogleClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl GoogleClient {
    /// Connects with an OAuth bearer access token, targeting the Google APIs root.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`), shared
    /// with the account's other providers.
    pub fn connect(token: impl Into<String>, tls: &TlsClientConfig) -> Result<Self, GoogleError> {
        let transport = Box::new(HttpTransport::new(token.into(), tls)?);
        Ok(Self::with_transport(transport, GOOGLE_BASE.to_owned()))
    }

    /// Connects a real client to a custom base origin instead of the Google root —
    /// e.g. a forward proxy, a regional endpoint, or a fixture-replay server in tests.
    /// Google returns opaque *tokens* (not absolute URLs), which the fetch layer
    /// re-attaches to base-relative paths, so link-following stays on this origin with
    /// no rebasing needed.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError::Transport`] if the HTTP client cannot be built.
    pub fn with_base(
        token: impl Into<String>,
        base: impl Into<String>,
        tls: &TlsClientConfig,
    ) -> Result<Self, GoogleError> {
        Ok(Self::with_transport(
            Box::new(HttpTransport::new(token.into(), tls)?),
            base.into(),
        ))
    }

    /// Wraps a transport and API root (the seam offline tests construct).
    pub(crate) fn with_transport(transport: Box<dyn GoogleTransport>, base: String) -> Self {
        Self { transport, base }
    }

    /// Builds an absolute URL from an API-relative path (`/gmail/v1/…`).
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Authenticated `GET`.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GoogleError`] (a non-2xx is [`GoogleError::Status`]).
    pub(crate) async fn get(&self, url: &str) -> Result<Value, GoogleError> {
        self.transport.get(url).await
    }

    /// Raw byte fetch, authenticated **only on the API origin**.
    ///
    /// Photo URLs reach this from the People payload (`photos[].url`), i.e. from
    /// remote content, and Google serves them off `googleusercontent.com` — a
    /// different origin that needs no token. Gating on the origin keeps the OAuth
    /// access token from travelling to whatever host a payload names, while every
    /// base-rooted API call authenticates exactly as before.
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GoogleError> {
        if engine_provider::same_origin(url, &self.base) {
            self.transport.get_bytes(url).await
        } else {
            self.transport.get_bytes_unauthenticated(url).await
        }
    }

    /// Authenticated `POST` of `body` with `content_type`. Returns the parsed JSON
    /// response body when the action echoed one (a `204` carries none).
    ///
    /// # Errors
    ///
    /// Returns a classified [`GoogleError`] (a non-2xx is [`GoogleError::Status`]).
    pub(crate) async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        self.transport.post(url, content_type, body).await
    }

    /// Authenticated `PATCH` guarded by `if_match`. Returns the updated object JSON.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GoogleError`] (a stale `If-Match` is a `412` conflict).
    pub(crate) async fn patch(
        &self,
        url: &str,
        content_type: &str,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        self.transport
            .patch(url, content_type, if_match, body)
            .await
    }

    /// Authenticated `DELETE` guarded by `if_match`.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GoogleError`] (a non-2xx is [`GoogleError::Status`]).
    pub(crate) async fn delete(
        &self,
        url: &str,
        if_match: Option<&str>,
    ) -> Result<(), GoogleError> {
        self.transport.delete(url, if_match).await
    }

    /// The HTTP version this client's transport negotiated, or `None` before its first
    /// request — [`connect`](Self::connect) performs no I/O, so a freshly connected
    /// Google client has not yet observed one. The matching TLS version is never
    /// available: reqwest exposes only the peer certificate
    /// (`docs/agent-guidance/tls.md`).
    pub(crate) fn http_version(&self) -> Option<HttpVersion> {
        self.transport.http_version()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_roots_urls_at_the_base_and_redacts_debug() {
        let client =
            GoogleClient::connect("super-secret-token", crate::test_support::tls()).unwrap();
        assert_eq!(
            client.url("/gmail/v1/users/me/labels"),
            format!("{GOOGLE_BASE}/gmail/v1/users/me/labels")
        );
        // A custom base roots every path there (a replay server / proxy).
        let custom =
            GoogleClient::with_base("t", "http://127.0.0.1:9", crate::test_support::tls()).unwrap();
        assert_eq!(
            custom.url("/calendar/v3/users/me/calendarList"),
            "http://127.0.0.1:9/calendar/v3/users/me/calendarList"
        );
        // The Debug rendering must not leak the bearer token.
        assert!(!format!("{client:?}").contains("super-secret-token"));
    }
}
