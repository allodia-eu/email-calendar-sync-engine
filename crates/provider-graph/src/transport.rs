//! The Microsoft Graph transport seam ([`GraphTransport`]) and connected client
//! ([`GraphClient`]).
//!
//! Graph has no session-discovery step (unlike JMAP): the API root is fixed and the
//! adapter just `GET`s absolute URLs (the v1.0 root for its own requests; the
//! `@odata.nextLink`/`@odata.deltaLink` URLs verbatim, since Graph returns them
//! absolute). A non-2xx response becomes a classified [`GraphError::Status`] with the
//! Graph error `code` extracted from the body.
//!
//! Requests carry `Prefer: IdType="ImmutableId"` so object ids are the immutable form —
//! stable across folder moves, the right `ProviderKey` for Graph mail.
//!
//! The [`GraphTransport`] seam lets the fetch/provider orchestration be unit-tested
//! offline against captured fixtures; the production reqwest implementation
//! ([`HttpTransport`](crate::http_transport)) lives in `http_transport`.

use std::sync::Arc;

use async_trait::async_trait;
use engine_http::RetryConfig;
use engine_provider::{
    Credential, CredentialSource, FixedCredential, HandOver, HttpVersion, TlsVersion,
};
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{error::GraphError, http_transport::HttpTransport, principal::MailboxPrincipal};

/// The Microsoft Graph v1.0 API root.
pub(crate) const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// An authenticated `GET` of an absolute Graph URL.
///
/// Implemented by [`HttpTransport`](crate::http_transport) (live reqwest) and, in tests,
/// by a fake fed canned fixtures keyed by URL — so the whole fetch orchestration runs
/// offline.
#[async_trait]
pub(crate) trait GraphTransport: Send + Sync {
    /// Fetches `url`, returning the parsed JSON or a classified error.
    async fn get(&self, url: &str) -> Result<Value, GraphError>;

    /// Fetches `url` with an extra `Prefer` value (the calendar read's
    /// `outlook.timezone="<IANA>"`, so Graph returns event times in that zone, or a mail
    /// delta page's `odata.maxpagesize`). The
    /// default ignores it and falls back to [`get`](Self::get) — a fake serving canned
    /// fixtures has no header to honor; only the reqwest transport sends it.
    async fn get_with_prefer(&self, url: &str, prefer: Option<&str>) -> Result<Value, GraphError> {
        let _ = prefer;
        self.get(url).await
    }

    /// Fetches `url`, returning the raw response bytes — for the `$value` endpoint
    /// that streams a message's RFC 822 MIME rather than JSON.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError>;

    /// Fetches raw bytes **without** the account's OAuth token, for a URL that came
    /// from remote content rather than from the Graph root — a contact payload can
    /// carry a photo URI naming any host, and the token must not travel there.
    async fn get_bytes_unauthenticated(&self, url: &str) -> Result<Vec<u8>, GraphError>;

    /// `POST`s `body` with `content_type` to `url`, returning the parsed JSON response
    /// body when the server sent one — an action that answers with an empty body
    /// (Graph `sendMail` returns `202 Accepted` with none) yields `None`. A non-2xx
    /// becomes a classified [`GraphError::Status`].
    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError>;

    /// [`post`](Self::post) for the one request that submits a message (`sendMail`): its reply
    /// waits the submission bound, and its failure after it may have reached the server
    /// leaves the send ambiguous (`crate::submit`). A fake has no wait to make, so it posts.
    async fn submit(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        hand_over: &HandOver<'_>,
    ) -> Result<Option<Value>, GraphError> {
        hand_over
            .commit()
            .await
            .map_err(|err| engine_http::SendError::withheld(err.detail().to_owned()))?;
        self.post(url, content_type, body).await
    }

    /// `PATCH`es `body` with `content_type` to `url`, guarded by `if_match` (an
    /// `If-Match` ETag precondition; a stale one is `412` → [`FailureClass::Conflict`]).
    /// Returns the updated object's JSON (Graph echoes it). A non-2xx becomes a
    /// classified [`GraphError::Status`].
    ///
    /// [`FailureClass::Conflict`]: engine_core::error::FailureClass::Conflict
    async fn patch(
        &self,
        url: &str,
        content_type: &str,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError>;

    /// `DELETE`s `url`, guarded by `if_match`. A `2xx` (Graph answers `204`) is success;
    /// a non-2xx becomes a classified [`GraphError::Status`] (a `404` — already gone —
    /// is the caller's to treat as idempotent success).
    async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GraphError>;

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

/// A connected Microsoft Graph client: an authenticated transport plus the API root.
///
/// Built with [`GraphClient::connect_with_source`], which asks the account's
/// [`CredentialSource`] for its token on every request, or with [`GraphClient::connect`] for a
/// token that never changes. The engine stays OAuth-agnostic: acquiring and refreshing the token
/// is the source's job (`docs/design/credential-source.md`). The fetch layer builds
/// Graph-relative paths and `GET`s them through the crate-internal `url`/`get` methods.
///
/// Cheap to clone, and the clones share one transport: one HTTP connection pool, one source,
/// and the versions it observed. A host builds one client per account and hands each
/// folder-bound [`GraphProvider`](crate::GraphProvider) a clone.
#[derive(Clone)]
pub struct GraphClient {
    transport: Arc<dyn GraphTransport>,
    base: String,
    principal: MailboxPrincipal,
}

impl core::fmt::Debug for GraphClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphClient")
            .field("base", &self.base)
            .field("principal", &self.principal)
            .finish_non_exhaustive()
    }
}

impl GraphClient {
    /// Connects with an OAuth bearer access token that never changes and is never replaced
    /// ([`FixedCredential`] under a subject of its own), targeting the Graph v1.0 root. A host
    /// whose token is refreshed uses [`connect_with_source`](Self::connect_with_source).
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`) and `retry` its
    /// throttling policy (`docs/agent-guidance/http-throttling.md`), both shared with the
    /// account's other providers; `user_agent` names the host's application in every request.
    pub fn connect(
        token: impl Into<String>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GraphError> {
        Self::connect_with_source(fixed(token), tls, retry, user_agent)
    }

    /// Connects to the Graph v1.0 root, asking `source` for the token on every request
    /// (`docs/design/credential-source.md`). Nothing is sent until the first request.
    ///
    /// Requests follow no redirect, so the token goes only where a request is addressed; it goes
    /// over plaintext only to loopback from a base on loopback, and only to an origin the source
    /// [permits](CredentialSource::permits). A `401` whose challenge offers `Bearer` asks the
    /// source once for a replacement and sends the request once more; a request with no answer
    /// is never sent again. The source must answer a token
    /// ([`Credential::Bearer`](engine_provider::Credential::Bearer)): Graph signs in with
    /// nothing else, so any other kind fails the request as `Permanent` before it is sent. A
    /// token with application permissions is one such token, and the client reaches a mailbox
    /// other than the signed-in user's through [`with_principal`](Self::with_principal).
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    pub fn connect_with_source(
        source: Arc<dyn CredentialSource>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GraphError> {
        Self::with_base_and_source(source, GRAPH_BASE, tls, retry, user_agent)
    }

    /// Connects to one specific mailbox the signed-in user can access — their own
    /// (`MailboxPrincipal::Me`) or a shared/other mailbox
    /// ([`MailboxPrincipal::user`]). One credential (the same `token`) backs every
    /// mailbox; each is a separate engine account differing only by this principal,
    /// which roots the client's requests at `/me` or `/users/{address}`
    /// (`principal.rs`).
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    pub fn for_mailbox(
        token: impl Into<String>,
        principal: MailboxPrincipal,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GraphError> {
        Ok(Self::connect(token, tls, retry, user_agent)?.with_principal(principal))
    }

    /// Connects a real client to a custom base origin instead of the Graph root —
    /// e.g. a forward proxy, a regional/sovereign endpoint, or a fixture-replay
    /// server in tests. Absolute `graph.microsoft.com` links Graph returns
    /// (`@odata.nextLink`/`deltaLink`) are rebased onto this origin, so
    /// link-following stays on the chosen endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    pub fn with_base(
        token: impl Into<String>,
        base: impl Into<String>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GraphError> {
        Self::with_base_and_source(fixed(token), &base.into(), tls, retry, user_agent)
    }

    /// [`with_base`](Self::with_base), asking `source` for the token on every request as
    /// [`connect_with_source`](Self::connect_with_source) does. The base is the account's
    /// configured origin: the token goes over plaintext only when it is on loopback.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Protocol`] if `base` is not an absolute URL, and
    /// [`GraphError::Transport`] if the HTTP client cannot be built.
    pub fn with_base_and_source(
        source: Arc<dyn CredentialSource>,
        base: &str,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GraphError> {
        Ok(Self::with_transport(
            Box::new(HttpTransport::new(base, source, tls, retry, user_agent)?),
            base.to_owned(),
        ))
    }

    /// Wraps a transport and API root (the seam offline tests construct),
    /// defaulting to the signed-in user's own mailbox.
    pub(crate) fn with_transport(transport: Box<dyn GraphTransport>, base: String) -> Self {
        Self {
            transport: Arc::from(transport),
            base,
            principal: MailboxPrincipal::Me,
        }
    }

    /// Roots this client's requests at a specific mailbox (the user's own, or a
    /// shared one) instead of `/me`: [`for_mailbox`](Self::for_mailbox) for a client asking a
    /// source. The transport is the same, so a clone rooted at another mailbox presents the same
    /// token.
    #[must_use]
    pub fn with_principal(mut self, principal: MailboxPrincipal) -> Self {
        self.principal = principal;
        self
    }

    /// Builds an absolute URL from a mailbox-relative path (`/mailFolders/…`),
    /// rooting it at the principal (`/me` or `/users/{address}`).
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}{path}", self.base, self.principal.root())
    }

    /// Builds an absolute URL rooted at a mailbox **other than** this client's — the
    /// shared-mailbox probe, which asks about an address the client is not bound to, and
    /// then about the signed-in user's own (`crate::shared`).
    pub(crate) fn principal_url(&self, principal: &MailboxPrincipal, path: &str) -> String {
        format!("{}{}{path}", self.base, principal.root())
    }

    /// The mailbox this client's requests are rooted at — read by submission, which must
    /// not send a message whose `From` names another mailbox than the one it posts to
    /// (`crate::submit`).
    pub(crate) fn principal(&self) -> &MailboxPrincipal {
        &self.principal
    }

    /// Builds an absolute Graph URL that is not rooted at a mailbox principal
    /// (organization contacts and directory users).
    pub(crate) fn global_url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Builds a mailbox-rooted URL against the **beta** API version.
    ///
    /// Only [`crate::report`] uses this, and only because it has no choice: reporting a
    /// message has no v1.0 endpoint at all. Its v1.0 predecessors `markAsJunk` /
    /// `markAsNotJunk` are deprecated and stopped returning data on 2025-12-30, and
    /// `reportMessage` — the action Microsoft's own deprecation notice points to — is
    /// beta-only. So this is not reaching for a preview feature; it is the only door.
    ///
    /// A base that is not the production root (a test replay server) is left alone, so
    /// the route still resolves.
    pub(crate) fn beta_url(&self, path: &str) -> String {
        let root = match self.base.strip_suffix("/v1.0") {
            Some(stem) => format!("{stem}/beta"),
            None => self.base.clone(),
        };
        format!("{root}{}{path}", self.principal.root())
    }

    /// Authenticated `GET`, rebasing absolute Graph links onto a non-default base.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a non-2xx is [`GraphError::Status`]).
    pub(crate) async fn get(&self, url: &str) -> Result<Value, GraphError> {
        self.transport.get(&self.rebase(url)).await
    }

    /// Authenticated `GET` with an extra `Prefer` value (a calendar read's
    /// `outlook.timezone`, a mail delta page's `odata.maxpagesize`), rebasing absolute Graph links
    /// like [`get`](Self::get).
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a non-2xx is [`GraphError::Status`]).
    pub(crate) async fn get_with_prefer(
        &self,
        url: &str,
        prefer: Option<&str>,
    ) -> Result<Value, GraphError> {
        self.transport
            .get_with_prefer(&self.rebase(url), prefer)
            .await
    }

    /// Authenticated `GET` returning the raw response bytes (the `$value` MIME
    /// stream), rebasing absolute Graph links onto a non-default base like [`get`].
    ///
    /// Authenticated **only on the Graph origin**: the `$value` MIME and
    /// `/photo/$value` endpoints are base-rooted and carry the token as before, but a
    /// contact payload can name any host in a photo URI. Sending the OAuth token there
    /// would hand the account's credentials to whoever the payload names, so an
    /// off-origin URL is fetched anonymously.
    ///
    /// [`get`]: Self::get
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a non-2xx is [`GraphError::Status`]).
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        let url = self.rebase(url);
        if engine_provider::same_origin(&url, &self.base) {
            self.transport.get_bytes(&url).await
        } else {
            self.transport.get_bytes_unauthenticated(&url).await
        }
    }

    /// Authenticated `POST` of `body` with `content_type`, rebasing absolute Graph
    /// links onto a non-default base like [`get`]. Returns the parsed JSON response
    /// body when the action echoed one (a `202`/`204` carries none).
    ///
    /// [`get`]: Self::get
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a non-2xx is [`GraphError::Status`]).
    pub(crate) async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        self.transport
            .post(&self.rebase(url), content_type, body)
            .await
    }

    /// [`post`](Self::post) for the request that submits a message (`sendMail`).
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
    ) -> Result<Option<Value>, GraphError> {
        self.transport
            .submit(&self.rebase(url), content_type, body, hand_over)
            .await
    }

    /// Authenticated `PATCH` guarded by `if_match`, rebasing links like [`get`]. Returns
    /// the updated object JSON Graph echoes.
    ///
    /// [`get`]: Self::get
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a stale `If-Match` is a `412` conflict).
    pub(crate) async fn patch(
        &self,
        url: &str,
        content_type: &str,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        self.transport
            .patch(&self.rebase(url), content_type, if_match, body)
            .await
    }

    /// Authenticated `DELETE` guarded by `if_match`, rebasing links like [`get`].
    ///
    /// [`get`]: Self::get
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a stale `If-Match` is a `412` conflict).
    pub(crate) async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GraphError> {
        self.transport.delete(&self.rebase(url), if_match).await
    }

    /// The HTTP version this client's transport negotiated, or `None` before its first
    /// request — [`connect`](Self::connect) performs no I/O, so a freshly connected
    /// Graph client has not yet observed one.
    pub(crate) fn http_version(&self) -> Option<HttpVersion> {
        self.transport.http_version()
    }

    /// The TLS version this client's transport negotiated, `None` until the first
    /// response for the same reason (`docs/agent-guidance/tls.md`).
    pub(crate) fn tls_version(&self) -> Option<TlsVersion> {
        self.transport.tls_version()
    }

    /// Rebases an absolute `graph.microsoft.com` URL onto a non-default base — a
    /// no-op in production (where `base` *is* the Graph root), so a proxy or a test
    /// replay server can catch the absolute `@odata` links Graph returns.
    fn rebase(&self, url: &str) -> String {
        match url.strip_prefix(GRAPH_BASE) {
            Some(rest) if self.base != GRAPH_BASE => format!("{}{rest}", self.base),
            _ => url.to_owned(),
        }
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
