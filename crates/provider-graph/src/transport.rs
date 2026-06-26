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
use serde_json::Value;

use crate::error::GraphError;
use crate::principal::MailboxPrincipal;

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
}

/// The production reqwest transport: bearer auth + immutable-id preference.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    token: String,
}

impl HttpTransport {
    /// Builds a transport authenticating with an OAuth bearer access token.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    pub(crate) fn new(token: String) -> Result<Self, GraphError> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            token,
        })
    }
}

#[async_trait]
impl GraphTransport for HttpTransport {
    async fn get(&self, url: &str) -> Result<Value, GraphError> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("Prefer", "IdType=\"ImmutableId\"")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.json::<Value>().await?)
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
    pub fn connect(token: impl Into<String>) -> Result<Self, GraphError> {
        let transport = Box::new(HttpTransport::new(token.into())?);
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
    ) -> Result<Self, GraphError> {
        Ok(Self::connect(token)?.with_principal(principal))
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
    ) -> Result<Self, GraphError> {
        Ok(Self::with_transport(
            Box::new(HttpTransport::new(token.into())?),
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
    use engine_core::error::FailureClass;
    use std::io::{Read, Write};

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
        let transport = HttpTransport::new("tok".to_owned()).unwrap();
        let doc = transport.get(&base).await.unwrap();
        assert!(doc.get("value").is_some());
    }

    #[tokio::test]
    async fn non_success_status_becomes_a_classified_status_error() {
        let body = r#"{"error":{"code":"InvalidAuthenticationToken","message":"nope"}}"#;
        let base = mock_server(http("401 Unauthorized", body));
        let err = HttpTransport::new("tok".to_owned())
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
        let err = HttpTransport::new("tok".to_owned())
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
        let err = HttpTransport::new("tok".to_owned())
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
        let me = GraphClient::connect("super-secret-token").unwrap();
        assert_eq!(me.url("/messages"), format!("{GRAPH_BASE}/me/messages"));
        // A shared mailbox roots requests at /users/{address} — the documented shape
        // `…/users/info@company.org/mailFolders('Inbox')/messages`.
        let shared =
            GraphClient::for_mailbox("t", MailboxPrincipal::user("info@company.org")).unwrap();
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
        let prod = GraphClient::connect("t").unwrap();
        let link = format!("{GRAPH_BASE}/me/messages/delta?$deltatoken=x");
        assert_eq!(prod.rebase(&link), link);
        // A custom base catches the absolute link (a replay server / proxy) …
        let custom = GraphClient::with_base("t", "http://127.0.0.1:9").unwrap();
        assert_eq!(
            custom.rebase(&link),
            "http://127.0.0.1:9/me/messages/delta?$deltatoken=x"
        );
        // … but only `graph.microsoft.com` links; anything else passes through.
        assert_eq!(custom.rebase("http://elsewhere/x"), "http://elsewhere/x");
    }
}
