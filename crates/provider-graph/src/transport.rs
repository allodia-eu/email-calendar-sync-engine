//! Bearer-authenticated HTTP transport for Microsoft Graph.
//!
//! Graph has no session-discovery step (unlike JMAP): the API root is fixed and the
//! adapter just `GET`s absolute URLs (the v1.0 root for its own requests; the
//! `@odata.nextLink`/`@odata.deltaLink` URLs verbatim, since Graph returns them
//! absolute). A non-2xx response becomes a classified [`GraphError::Status`] with
//! the Graph error `code` extracted from the body.
//!
//! Requests carry `Prefer: IdType="ImmutableId"` so object ids are the immutable
//! form — stable across folder moves, the right `ProviderKey` for Graph mail.
//!
//! The [`GraphTransport`] seam lets the fetch/provider orchestration be unit-tested
//! offline against captured fixtures; [`HttpTransport`] is the production reqwest
//! implementation.

use async_trait::async_trait;
use engine_provider::{HttpVersion, ObservedHttpVersion};
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{error::GraphError, principal::MailboxPrincipal};

/// The Microsoft Graph v1.0 API root.
pub(crate) const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// An authenticated `GET` of an absolute Graph URL.
///
/// Implemented by [`HttpTransport`] (live reqwest) and, in tests, by a fake fed
/// canned fixtures keyed by URL — so the whole fetch orchestration runs offline.
#[async_trait]
pub(crate) trait GraphTransport: Send + Sync {
    /// Fetches `url`, returning the parsed JSON or a classified error.
    async fn get(&self, url: &str) -> Result<Value, GraphError>;

    /// Fetches `url`, returning the raw response bytes — for the `$value` endpoint
    /// that streams a message's RFC 822 MIME rather than JSON.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError>;

    /// The HTTP version the transport negotiated, or `None` before its first response.
    /// Defaults to `None`: only [`HttpTransport`] speaks HTTP, so a fake fed canned
    /// fixtures has no version to report.
    fn http_version(&self) -> Option<HttpVersion> {
        None
    }
}

/// The production reqwest transport: bearer auth + immutable-id preference.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    token: String,
    /// The HTTP version most recently observed. Unlike JMAP/CalDAV,
    /// [`GraphClient::connect`] performs no request (Graph has no session-discovery
    /// step), so this stays `None` until the adapter's first fetch.
    http_version: ObservedHttpVersion,
}

impl HttpTransport {
    /// Builds a transport authenticating with an OAuth bearer access token.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`).
    pub(crate) fn new(token: String, tls: &TlsClientConfig) -> Result<Self, GraphError> {
        Ok(Self {
            client: tls.reqwest_builder().build()?,
            token,
            http_version: ObservedHttpVersion::default(),
        })
    }

    /// Issues the authenticated, immutable-id-preferring `GET` both fetch shapes share,
    /// recording the negotiated HTTP version on the way through — the one funnel, so no
    /// path can forget to observe it.
    async fn send(&self, url: &str) -> Result<reqwest::Response, GraphError> {
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("Prefer", "IdType=\"ImmutableId\"")
            .send()
            .await?;
        self.http_version.record(response.version());
        Ok(response)
    }
}

#[async_trait]
impl GraphTransport for HttpTransport {
    async fn get(&self, url: &str) -> Result<Value, GraphError> {
        let resp = self.send(url).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.json::<Value>().await?)
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        let resp = self.send(url).await?;
        let status = resp.status();
        if !status.is_success() {
            // The `$value` error body is JSON like any other Graph error, so classify it
            // the same way (an expired/moved message → the caller re-syncs and retries).
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    fn http_version(&self) -> Option<HttpVersion> {
        self.http_version.get()
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

    /// Authenticated `GET`, rebasing absolute Graph links onto a non-default base.
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a non-2xx is [`GraphError::Status`]).
    pub(crate) async fn get(&self, url: &str) -> Result<Value, GraphError> {
        self.transport.get(&self.rebase(url)).await
    }

    /// Authenticated `GET` returning the raw response bytes (the `$value` MIME
    /// stream), rebasing absolute Graph links onto a non-default base like [`get`].
    ///
    /// [`get`]: Self::get
    ///
    /// # Errors
    ///
    /// Returns a classified [`GraphError`] (a non-2xx is [`GraphError::Status`]).
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        self.transport.get_bytes(&self.rebase(url)).await
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
    use std::io::{Read, Write};

    use engine_core::error::FailureClass;

    use super::*;

    /// A blocking single-shot mock HTTP server: serves `response` to one
    /// connection, so the live reqwest transport runs offline (no network).
    fn mock_server(response: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn http(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn get_parses_a_success_body() {
        let base = mock_server(http("200 OK", r#"{"value":[]}"#));
        let transport = HttpTransport::new("tok".to_owned(), crate::test_support::tls()).unwrap();
        let doc = transport.get(&base).await.unwrap();
        assert!(doc.get("value").is_some());
    }

    #[tokio::test]
    async fn the_http_version_is_unknown_until_the_first_response() {
        // Graph's connect performs no I/O (no session discovery), so a fresh transport
        // has observed nothing; the first response fills the fact in. This is the one
        // provider where `ConnectionInfo::http_version` is `None` on a live connection.
        let base = mock_server(http("200 OK", r#"{"value":[]}"#));
        let transport = HttpTransport::new("tok".to_owned(), crate::test_support::tls()).unwrap();
        assert_eq!(GraphTransport::http_version(&transport), None);

        transport.get(&base).await.unwrap();
        assert_eq!(
            GraphTransport::http_version(&transport),
            Some(HttpVersion::Http1_1)
        );
    }

    #[tokio::test]
    async fn get_bytes_returns_the_raw_body_verbatim() {
        // `$value` is not JSON — it is the raw RFC 822 MIME; get_bytes returns it as-is.
        let mime = "From: a@example.com\r\nSubject: Hi\r\n\r\nBody\r\n";
        let base = mock_server(http("200 OK", mime));
        let bytes = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get_bytes(&base)
            .await
            .unwrap();
        assert_eq!(bytes, mime.as_bytes());
    }

    #[tokio::test]
    async fn get_bytes_non_success_is_a_classified_status_error() {
        // A gone/moved message → the same status classification as any Graph error.
        let body = r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#;
        let base = mock_server(http("404 Not Found", body));
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get_bytes(&base)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, GraphError::Status { code: Some(c), .. } if c == "ErrorItemNotFound")
        );
    }

    #[tokio::test]
    async fn non_success_status_becomes_a_classified_status_error() {
        let body = r#"{"error":{"code":"InvalidAuthenticationToken","message":"nope"}}"#;
        let base = mock_server(http("401 Unauthorized", body));
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get(&base)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, GraphError::Status { code: Some(c), .. } if c == "InvalidAuthenticationToken")
        );
        assert_eq!(err.failure_class(), FailureClass::Authentication);
    }

    #[tokio::test]
    async fn a_non_json_success_body_is_a_permanent_decode_error() {
        let base = mock_server(http("200 OK", "this is not json"));
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get(&base)
            .await
            .unwrap_err();
        // A body that does not decode is a permanent protocol mismatch.
        assert!(matches!(err, GraphError::Transport(_)));
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }

    #[tokio::test]
    async fn a_refused_connection_is_a_retryable_transport_error() {
        // Nothing is listening on this port → reqwest connect error → retryable.
        let err = HttpTransport::new("tok".to_owned(), crate::test_support::tls())
            .unwrap()
            .get("http://127.0.0.1:1/me")
            .await
            .unwrap_err();
        assert!(matches!(err, GraphError::Transport(_)));
        assert!(err.failure_class().is_retryable());
    }

    #[test]
    fn client_roots_urls_at_the_principal_and_redacts_debug() {
        // Default — the signed-in user's own mailbox roots at /me.
        let me = GraphClient::connect("super-secret-token", crate::test_support::tls()).unwrap();
        assert_eq!(me.url("/messages"), format!("{GRAPH_BASE}/me/messages"));
        // A shared mailbox roots requests at /users/{address} — the documented shape
        // `…/users/info@company.org/mailFolders('Inbox')/messages`.
        let shared = GraphClient::for_mailbox(
            "t",
            MailboxPrincipal::user("info@company.org"),
            crate::test_support::tls(),
        )
        .unwrap();
        assert_eq!(
            shared.url("/mailFolders('Inbox')/messages"),
            format!("{GRAPH_BASE}/users/info@company.org/mailFolders('Inbox')/messages")
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
