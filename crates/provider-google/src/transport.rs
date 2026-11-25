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

use std::sync::Arc;

use async_trait::async_trait;
use engine_http::RetryConfig;
use engine_provider::{
    Credential, CredentialSource, CredentialSubject, FixedCredential, HandOver, HttpVersion,
    TlsVersion,
};
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{error::GoogleError, http_transport::HttpTransport};

/// The universal Google APIs host — serves both `gmail/v1/…` and `calendar/v3/…`.
pub(crate) const GOOGLE_BASE: &str = "https://www.googleapis.com";

/// The People API host. Unlike Gmail and Calendar, People is **not** served from
/// [`GOOGLE_BASE`]: both `www.googleapis.com/v1/people/…` and the service-prefixed
/// `www.googleapis.com/people/v1/…` answer an HTML `404`, so contact calls must be
/// rooted here instead.
pub(crate) const PEOPLE_BASE: &str = "https://people.googleapis.com";

/// The Meet REST API root, where `spaces.create` is served.
pub(crate) const MEET_BASE: &str = "https://meet.googleapis.com";

/// An authenticated request against a Google API.
///
/// Implemented by [`HttpTransport`](crate::http_transport) (live reqwest) and, in
/// tests, by a fake fed canned fixtures keyed by URL — so the whole fetch
/// orchestration runs offline.
#[async_trait]
pub(crate) trait GoogleTransport: Send + Sync {
    /// Fetches `url`, returning the parsed JSON or a classified error. A `204` with no body is
    /// `null`: Gmail answers an empty list so (its filters and forwarding addresses, observed).
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

    /// [`post`](Self::post) for the one request that submits a message (`messages.send`): its reply
    /// waits the submission bound, and its failure after it may have reached the server
    /// leaves the send ambiguous (`crate::submit`). A fake has no wait to make, so it posts.
    async fn submit(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        hand_over: &HandOver<'_>,
    ) -> Result<Option<Value>, GoogleError> {
        hand_over
            .commit()
            .await
            .map_err(|err| engine_http::SendError::withheld(err.detail().to_owned()))?;
        self.post(url, content_type, body).await
    }

    /// `PUT`s `body` with `content_type` to `url`, returning the parsed JSON response.
    /// Gmail's `drafts.update` is the one caller: it **replaces** a draft's content while
    /// keeping the draft's own id, which is why it is a `PUT` and not the `PATCH` above.
    ///
    /// Defaults to rejecting, so a fake that no draft test drives implements nothing.
    ///
    /// # Errors
    ///
    /// A classified [`GoogleError`]; the default is [`GoogleError::Protocol`].
    async fn put(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        let _ = (url, content_type, body);
        Err(GoogleError::protocol("transport does not implement PUT"))
    }

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

    /// The TLS version the transport negotiated, `None` for the same reason — and also
    /// before the first response, since it arrives on one.
    fn tls_version(&self) -> Option<TlsVersion> {
        None
    }
}

/// A connected Google client: an authenticated transport plus the API root.
///
/// Built with [`GoogleClient::connect_with_source`], which asks the account's
/// [`CredentialSource`] for its token on every request, or with [`GoogleClient::connect`] for a
/// token that never changes. The engine stays OAuth-agnostic: acquiring and refreshing the token
/// is the source's job (`docs/design/credential-source.md`). The fetch layer builds API-relative
/// paths and issues them through the crate-internal `url`/`get`/… methods.
///
/// Cheap to clone, and the clones share one transport: one HTTP connection pool, one source,
/// one request gate, and the versions it observed. A host builds one client per account and
/// hands each of the account's providers a clone.
#[derive(Clone)]
pub struct GoogleClient {
    transport: Arc<dyn GoogleTransport>,
    base: String,
    /// Who the source signs in as; `None` only for a transport a test builds.
    subject: Option<CredentialSubject>,
}

impl core::fmt::Debug for GoogleClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GoogleClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// Percent-encodes one **query-parameter value**.
///
/// Continuation tokens (`pageToken`, `syncToken`, `startHistoryId`) are opaque
/// server-generated strings that this crate splices into a query string. Splicing
/// them raw means a token containing `&`, `#`, or `=` re-parameterizes or truncates
/// the request — the caller would silently fetch a different page than the server
/// named. Encoding everything outside the RFC 3986 unreserved set keeps the token a
/// value, never syntax.
pub(crate) fn encode_query_value(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

impl GoogleClient {
    /// Connects with an OAuth bearer access token that never changes and is never replaced
    /// ([`FixedCredential`] under a subject of its own), targeting the Google APIs root. A host
    /// whose token is refreshed, or whose account also reads mail over IMAP, uses
    /// [`connect_with_source`](Self::connect_with_source).
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`) and `retry` its
    /// throttling policy (`docs/agent-guidance/http-throttling.md`), both shared with the
    /// account's other providers; `user_agent` names the host's application in every request.
    pub fn connect(
        token: impl Into<String>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GoogleError> {
        Self::connect_with_source(fixed(token), tls, retry, user_agent)
    }

    /// Connects to the Google APIs, asking `source` for the token on every request
    /// (`docs/design/credential-source.md`). Nothing is sent until the first request.
    ///
    /// Requests follow no redirect, so the token goes only where a request is addressed; it goes
    /// over plaintext only to loopback from a base on loopback, and only to an origin the source
    /// [permits](CredentialSource::permits) (Gmail and Calendar at `www.googleapis.com`, People
    /// and Meet at hosts of their own). A `401` whose challenge offers `Bearer` asks the source
    /// once for a replacement and sends the request once more; a request with no answer is never
    /// sent again. The source must answer a token
    /// ([`Credential::Bearer`](engine_provider::Credential::Bearer)): the APIs sign in with
    /// nothing else, so any other kind fails the request as `Permanent` before it is sent. A
    /// domain-wide-delegation token minted for one user is one such token.
    ///
    /// The account's IMAP half, where there is one, asks the same source
    /// ([`GmailProvider::with_imap`](crate::GmailProvider::with_imap)); a token answered with
    /// the account's address ([`Credential::bearer_for`]) serves both.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError::Transport`] if the HTTP client cannot be built.
    pub fn connect_with_source(
        source: Arc<dyn CredentialSource>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GoogleError> {
        Self::with_base_and_source(source, GOOGLE_BASE, tls, retry, user_agent)
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
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GoogleError> {
        Self::with_base_and_source(fixed(token), &base.into(), tls, retry, user_agent)
    }

    /// [`with_base`](Self::with_base), asking `source` for the token on every request as
    /// [`connect_with_source`](Self::connect_with_source) does. The base is the account's
    /// configured origin: the token goes over plaintext only when it is on loopback.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError::Protocol`] if `base` is not an absolute URL, and
    /// [`GoogleError::Transport`] if the HTTP client cannot be built.
    pub fn with_base_and_source(
        source: Arc<dyn CredentialSource>,
        base: &str,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GoogleError> {
        let subject = source.subject().clone();
        let transport = HttpTransport::new(base, source, tls, retry, user_agent)?;
        Ok(Self {
            subject: Some(subject),
            ..Self::with_transport(Box::new(transport), base.to_owned())
        })
    }

    /// Wraps a transport and API root (the seam offline tests construct).
    pub(crate) fn with_transport(transport: Box<dyn GoogleTransport>, base: String) -> Self {
        Self {
            transport: Arc::from(transport),
            base,
            subject: None,
        }
    }

    /// Who the account's source signs in as; `None` only for a transport a test builds.
    pub(crate) fn subject(&self) -> Option<&CredentialSubject> {
        self.subject.as_ref()
    }

    /// Builds an absolute URL from an API-relative path (`/gmail/v1/…`).
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Builds an absolute **People API** URL from an API-relative path (`/v1/people/…`).
    ///
    /// People is not served from [`GOOGLE_BASE`] (see [`PEOPLE_BASE`]), so contact paths are
    /// rooted separately. A client built with a custom base — a replay server or a proxy —
    /// still wins, so the offline tests and any host-supplied origin keep working unchanged.
    pub(crate) fn people_url(&self, path: &str) -> String {
        self.url_at(PEOPLE_BASE, path)
    }

    /// Builds an absolute **Meet API** URL from an API-relative path (`/v2/spaces`). Meet has
    /// a host of its own ([`MEET_BASE`]), and a client built with a custom base keeps it.
    pub(crate) fn meet_url(&self, path: &str) -> String {
        self.url_at(MEET_BASE, path)
    }

    /// `path` at `host`, an API's own host, unless this client was built with a custom base.
    fn url_at(&self, host: &str, path: &str) -> String {
        if self.base == GOOGLE_BASE {
            format!("{host}{path}")
        } else {
            format!("{}{path}", self.base)
        }
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

    /// [`post`](Self::post) for the request that submits a message (`messages.send`).
    ///
    /// # Errors
    ///
    /// As [`post`](Self::post).
    pub(crate) async fn submit(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        hand_over: &HandOver<'_>,
    ) -> Result<Option<Value>, GoogleError> {
        self.transport
            .submit(url, content_type, body, hand_over)
            .await
    }

    /// Authenticated `PUT`. Returns the replaced object's JSON.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GoogleError`] (a non-2xx is [`GoogleError::Status`]).
    pub(crate) async fn put(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        self.transport.put(url, content_type, body).await
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
    /// Google client has not yet observed one.
    pub(crate) fn http_version(&self) -> Option<HttpVersion> {
        self.transport.http_version()
    }

    /// The TLS version this client's transport negotiated, `None` until the first
    /// response for the same reason (`docs/agent-guidance/tls.md`).
    pub(crate) fn tls_version(&self) -> Option<TlsVersion> {
        self.transport.tls_version()
    }
}

/// `token` as a source that never changes and is never replaced, under a subject of its own.
fn fixed(token: impl Into<String>) -> Arc<dyn CredentialSource> {
    Arc::new(FixedCredential::unique(Credential::bearer(token)))
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "credential_source_tests.rs"]
mod credential_source_tests;
