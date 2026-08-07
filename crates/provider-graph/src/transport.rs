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

use async_trait::async_trait;
use engine_provider::HttpVersion;
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
    /// `outlook.timezone="<IANA>"`, so Graph returns event times in that zone). The
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
}

/// A connected Microsoft Graph client: an authenticated transport plus the API root.
///
/// Built with [`GraphClient::connect`] (an OAuth bearer access token; the engine
/// stays OAuth-agnostic, so token acquisition/refresh is the host's job —
/// `north-star.md`). The fetch layer builds Graph-relative paths and `GET`s them
/// through the crate-internal `url`/`get` methods.
pub struct GraphClient {
    transport: Box<dyn GraphTransport>,
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
    /// Connects with an OAuth bearer access token, targeting the Graph v1.0 root.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`), shared
    /// with the account's other providers.
    pub fn connect(token: impl Into<String>, tls: &TlsClientConfig) -> Result<Self, GraphError> {
        let transport = Box::new(HttpTransport::new(token.into(), tls)?);
        Ok(Self::with_transport(transport, GRAPH_BASE.to_owned()))
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
    ) -> Result<Self, GraphError> {
        Ok(Self::connect(token, tls)?.with_principal(principal))
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
    ) -> Result<Self, GraphError> {
        Ok(Self::with_transport(
            Box::new(HttpTransport::new(token.into(), tls)?),
            base.into(),
        ))
    }

    /// Wraps a transport and API root (the seam offline tests construct),
    /// defaulting to the signed-in user's own mailbox.
    pub(crate) fn with_transport(transport: Box<dyn GraphTransport>, base: String) -> Self {
        Self {
            transport,
            base,
            principal: MailboxPrincipal::Me,
        }
    }

    /// Roots this client's requests at a specific mailbox (the user's own, or a
    /// shared one) instead of `/me`.
    #[must_use]
    pub(crate) fn with_principal(mut self, principal: MailboxPrincipal) -> Self {
        self.principal = principal;
        self
    }

    /// Builds an absolute URL from a mailbox-relative path (`/mailFolders/…`),
    /// rooting it at the principal (`/me` or `/users/{address}`).
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}{path}", self.base, self.principal.root())
    }

    /// Builds an absolute Graph URL that is not rooted at a mailbox principal
    /// (organization contacts and directory users).
    pub(crate) fn global_url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
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
    /// `outlook.timezone`), rebasing absolute Graph links like [`get`](Self::get).
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
    /// Graph client has not yet observed one. The matching TLS version is never
    /// available: reqwest exposes only the peer certificate
    /// (`docs/agent-guidance/tls.md`).
    pub(crate) fn http_version(&self) -> Option<HttpVersion> {
        self.transport.http_version()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_roots_urls_at_the_principal_and_redacts_debug() {
        // Default — the signed-in user's own mailbox roots at /me.
        let me = GraphClient::connect("super-secret-token", crate::test_support::tls()).unwrap();
        assert_eq!(me.url("/messages"), format!("{GRAPH_BASE}/me/messages"));
        // A shared mailbox roots requests at /users/{address} — the documented shape
        // `…/users/info@example.org/mailFolders('Inbox')/messages`.
        let shared = GraphClient::for_mailbox(
            "t",
            MailboxPrincipal::user("info@example.org"),
            crate::test_support::tls(),
        )
        .unwrap();
        assert_eq!(
            shared.url("/mailFolders('Inbox')/messages"),
            format!("{GRAPH_BASE}/users/info@example.org/mailFolders('Inbox')/messages")
        );
        // The Debug rendering must not leak the bearer token.
        assert!(!format!("{me:?}").contains("super-secret-token"));
    }

    #[test]
    fn rebase_targets_a_custom_base_but_is_a_noop_at_the_default() {
        // At the default base, an absolute Graph link is left untouched.
        let prod = GraphClient::connect("t", crate::test_support::tls()).unwrap();
        let link = format!("{GRAPH_BASE}/me/messages/delta?$deltatoken=x");
        assert_eq!(prod.rebase(&link), link);
        // A custom base catches the absolute link (a replay server / proxy) …
        let custom =
            GraphClient::with_base("t", "http://127.0.0.1:9", crate::test_support::tls()).unwrap();
        assert_eq!(
            custom.rebase(&link),
            "http://127.0.0.1:9/me/messages/delta?$deltatoken=x"
        );
        // … but only `graph.microsoft.com` links; anything else passes through.
        assert_eq!(custom.rebase("http://elsewhere/x"), "http://elsewhere/x");
    }
}
