//! The production reqwest [`GraphTransport`]: bearer auth + immutable-id preference.
//!
//! Every request that carries the token goes through one [`AuthenticatedClient`], which asks
//! the account's [`CredentialSource`] for it each time (`docs/design/credential-source.md`): a
//! token the host refreshed is the one presented, and a token Graph refuses with a `401` is
//! replaced once and the request sent once more. A content URL is fetched on a plain client,
//! with no token and no ask.
//!
//! The one funnel every request flows through records the negotiated HTTP and TLS
//! versions, so no path forgets to observe them. Reads add `Prefer: IdType="ImmutableId"` (and a
//! calendar read's `outlook.timezone`, a mail delta page's `odata.maxpagesize`); writes add an
//! optional `Content-Type`/`If-Match`. The offline tests drive it over a blocking single-shot mock
//! server (no network).

use std::sync::Arc;

use async_trait::async_trait;
use engine_http::{
    AuthenticatedClient, Exchange, ObservedConnection, RetryConfig, Sent, Unauthorized,
    send_retrying,
};
use engine_provider::{Credential, CredentialSource, HttpVersion, TlsVersion};
use engine_tls::TlsClientConfig;
use reqwest::RequestBuilder;
use serde_json::Value;

use crate::{
    error::GraphError, provider::MAX_CONCURRENT_SOURCE_FETCHES, transport::GraphTransport,
};

/// The production reqwest transport: bearer auth + immutable-id preference.
pub(crate) struct HttpTransport {
    /// The account's client, which asks its source for the token on every request.
    http: AuthenticatedClient,
    /// Content URLs, which never carry the token: a plain client, following redirects as any
    /// fetch of a public URL does.
    anonymous: reqwest::Client,
    /// The HTTP and TLS versions most recently observed. Unlike JMAP/CalDAV,
    /// `GraphClient::connect` performs no request (Graph has no session-discovery step),
    /// so these stay `None` until the adapter's first fetch.
    connection: ObservedConnection,
    /// How a `429` is waited out. Exchange Online throttles a mailbox on both concurrency
    /// and a requests-per-window budget, and names its own wait in `Retry-After`.
    retry: RetryConfig,
}

impl HttpTransport {
    /// Builds a transport for the account configured at `base`, asking `source` for its token
    /// on every request.
    ///
    /// # Errors
    ///
    /// [`GraphError::Protocol`] if `base` is not an absolute URL; [`GraphError::Transport`] if
    /// the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`) and `retry` its
    /// throttling policy (`docs/agent-guidance/http-throttling.md`).
    pub(crate) fn new(
        base: &str,
        source: Arc<dyn CredentialSource>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GraphError> {
        let retry = retry.clone().labelled("graph");
        // What Exchange Online allows one mailbox, stated once for the whole account: the
        // gate is the host's, shared by every provider it connected for this account, and
        // the number is the adapter's because only the adapter knows the server
        // (`engine_http::RequestGate`). A folder-bound provider that bounded itself would
        // be 56 ceilings of four.
        retry.gate().narrow_to(MAX_CONCURRENT_SOURCE_FETCHES);
        // Not echoed: a host may have put a credential in it.
        let configured = reqwest::Url::parse(base)
            .map_err(|_| GraphError::protocol("the Graph base is not an absolute URL"))?;
        Ok(Self {
            http: AuthenticatedClient::new(tls, user_agent, configured, source, retry.clone())?,
            anonymous: engine_http::client(tls, user_agent).build()?,
            connection: ObservedConnection::default(),
            retry,
        })
    }

    /// Sends `request` with the account's token, through the account's client, and records the
    /// transport versions it was answered over.
    async fn authenticated(
        &self,
        request: RequestBuilder,
        exchange: Exchange<'_>,
    ) -> Result<Sent, GraphError> {
        let sent = self.http.send(request, exchange, present, judge).await?;
        self.connection.record(&sent);
        Ok(sent)
    }

    /// Issues the authenticated, immutable-id-preferring `GET` the fetch shapes share,
    /// recording the negotiated HTTP and TLS versions on the way through — the one
    /// funnel, so no path can forget them. `extra_prefer` appends a second `Prefer` value
    /// (the calendar read's `outlook.timezone`, a mail delta page's `odata.maxpagesize`).
    async fn send(
        &self,
        url: &str,
        extra_prefer: Option<&str>,
    ) -> Result<engine_http::Sent, GraphError> {
        let prefer = match extra_prefer {
            Some(extra) => format!("IdType=\"ImmutableId\", {extra}"),
            None => "IdType=\"ImmutableId\"".to_owned(),
        };
        let request = self.http.get(url).header("Prefer", prefer);
        self.authenticated(request, Exchange::Ordinary).await
    }

    /// Issues an authenticated write (`POST`/`PATCH`/`DELETE`) the write shapes share,
    /// recording the transport versions like [`send`](Self::send) — the write
    /// counterpart, so both funnels observe them. Carries the immutable-id
    /// preference so a write that echoes an object (a create/patch) returns the stable
    /// id form, plus an optional `Content-Type` and `If-Match` precondition.
    async fn send_write(
        &self,
        method: reqwest::Method,
        url: &str,
        content_type: Option<&str>,
        if_match: Option<&str>,
        body: Vec<u8>,
        exchange: Exchange<'_>,
    ) -> Result<engine_http::Sent, GraphError> {
        let mut request = self
            .http
            .request(method, url)
            .header("Prefer", "IdType=\"ImmutableId\"");
        if let Some(content_type) = content_type {
            request = request.header("Content-Type", content_type);
        }
        if let Some(if_match) = if_match {
            request = request.header("If-Match", if_match);
        }
        // A bodyless `POST` action (Graph `permanentDelete`) needs an explicit
        // `Content-Length: 0`: reqwest omits the header for an empty body, and Graph
        // answers such a `POST` with `411 Length Required`. (`DELETE` needs no length,
        // so the extra header is harmless there.)
        if body.is_empty() {
            request = request.header(reqwest::header::CONTENT_LENGTH, 0);
        }
        self.authenticated(request.body(body), exchange).await
    }

    /// Turns a byte response into its body, classifying a non-2xx. Shared by the
    /// authenticated and anonymous byte paths so they differ in exactly one thing:
    /// whether the bearer token is attached.
    async fn collect_bytes(resp: engine_http::Sent) -> Result<Vec<u8>, GraphError> {
        let status = resp.status();
        if !status.is_success() {
            // The `$value` error body is JSON like any other Graph error, so classify it
            // the same way (an expired/moved message → the caller re-syncs and retries).
            let wait = resp.stated_wait();
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body).with_retry_after(wait));
        }
        Ok(resp.bytes().await?)
    }
}

/// The token on `builder`: Graph signs in with a token alone, so any other kind is refused before
/// anything is sent.
fn present(builder: RequestBuilder, credential: &Credential) -> Result<RequestBuilder, GraphError> {
    Ok(engine_http::present_bearer(builder, credential)?)
}

/// What a `401` to a request that carried the token says: [`engine_http::judge_by_scheme`], as on
/// every adapter, so a refusal with a `Bearer` challenge or none at all (YAHOO-1) is replaced
/// once. The one exception is the directory read with no challenge (MICROSOFT-2 in
/// `docs/spec-deviations.md`), which is final.
fn judge(reply: &Sent, credential: &Credential) -> Unauthorized {
    if is_absent_directory(reply) {
        Unauthorized::Final
    } else {
        engine_http::judge_by_scheme(reply, credential)
    }
}

/// Whether `reply` is Graph saying the account has no directory: a `401` with no challenge to
/// its delta (`/users/delta`, the first page or a link Graph handed back). Graph answers a
/// personal account's directory read that way with a good token, so a new token would be
/// refused the same; it refuses a token with a `Bearer` challenge. Only that read was observed,
/// so only that read is read this way.
fn is_absent_directory(reply: &Sent) -> bool {
    reply.url().path().ends_with("/users/delta")
        && engine_http::offered_schemes(reply.headers()).is_empty()
}

/// Turns a successful write response into its parsed JSON body, or `None` when the
/// action carried none (`202`/`204`). A non-2xx is a classified [`GraphError::Status`].
async fn write_body(resp: engine_http::Sent) -> Result<Option<Value>, GraphError> {
    let status = resp.status();
    if !status.is_success() {
        let wait = resp.stated_wait();
        let body = resp.text().await.unwrap_or_default();
        return Err(GraphError::status(status.as_u16(), body).with_retry_after(wait));
    }
    let text = resp.text().await.unwrap_or_default();
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&text)?))
    }
}

#[async_trait]
impl GraphTransport for HttpTransport {
    async fn get(&self, url: &str) -> Result<Value, GraphError> {
        let resp = self.send(url, None).await?;
        let status = resp.status();
        if !status.is_success() {
            let wait = resp.stated_wait();
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body).with_retry_after(wait));
        }
        Ok(serde_json::from_slice(&resp.bytes().await?)?)
    }

    async fn get_with_prefer(&self, url: &str, prefer: Option<&str>) -> Result<Value, GraphError> {
        let resp = self.send(url, prefer).await?;
        let status = resp.status();
        if !status.is_success() {
            let wait = resp.stated_wait();
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body).with_retry_after(wait));
        }
        Ok(serde_json::from_slice(&resp.bytes().await?)?)
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        let resp = self.send(url, None).await?;
        Self::collect_bytes(resp).await
    }

    async fn get_bytes_unauthenticated(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        // No `Authorization` and no `Prefer`: this URL came from payload content, not
        // from the Graph API, so it gets a bare GET.
        // Not recorded: the content host's connection says nothing about the account's.
        let resp = send_retrying(self.anonymous.get(url), &self.retry, Exchange::Ordinary).await?;
        Self::collect_bytes(resp).await
    }

    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        let post = reqwest::Method::POST;
        let resp = self
            .send_write(
                post,
                url,
                Some(content_type),
                None,
                body,
                Exchange::Ordinary,
            )
            .await?;
        write_body(resp).await
    }

    async fn submit(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        hand_over: &engine_provider::HandOver<'_>,
    ) -> Result<Option<Value>, GraphError> {
        let post = reqwest::Method::POST;
        let resp = self
            .send_write(
                post,
                url,
                Some(content_type),
                None,
                body,
                Exchange::Submission(hand_over),
            )
            .await?;
        write_body(resp).await
    }

    async fn patch(
        &self,
        url: &str,
        content_type: &str,
        if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        let resp = self
            .send_write(
                reqwest::Method::PATCH,
                url,
                Some(content_type),
                if_match,
                body,
                Exchange::Ordinary,
            )
            .await?;
        write_body(resp).await
    }

    async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GraphError> {
        let resp = self
            .send_write(
                reqwest::Method::DELETE,
                url,
                None,
                if_match,
                Vec::new(),
                Exchange::Ordinary,
            )
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let wait = resp.stated_wait();
            let body = resp.text().await.unwrap_or_default();
            Err(GraphError::status(status.as_u16(), body).with_retry_after(wait))
        }
    }

    fn http_version(&self) -> Option<HttpVersion> {
        self.connection.http_version()
    }

    fn tls_version(&self) -> Option<TlsVersion> {
        self.connection.tls_version()
    }
}

#[cfg(test)]
#[path = "http_transport_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod deadline_tests;
