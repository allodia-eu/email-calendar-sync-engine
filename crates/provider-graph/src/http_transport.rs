//! The production reqwest [`GraphTransport`]: bearer auth + immutable-id preference.
//!
//! The one funnel every request flows through records the negotiated HTTP and TLS
//! versions, so no path forgets to observe them. Reads add `Prefer: IdType="ImmutableId"` (and a
//! calendar read's `outlook.timezone`); writes add an optional `Content-Type`/`If-Match`.
//! The offline tests drive it over a blocking single-shot mock server (no network).

use async_trait::async_trait;
use engine_http::{ObservedConnection, RetryConfig, send_retrying};
use engine_provider::{HttpVersion, TlsVersion};
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{
    error::GraphError, provider::MAX_CONCURRENT_SOURCE_FETCHES, transport::GraphTransport,
};

/// The production reqwest transport: bearer auth + immutable-id preference.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    token: String,
    /// The HTTP and TLS versions most recently observed. Unlike JMAP/CalDAV,
    /// `GraphClient::connect` performs no request (Graph has no session-discovery step),
    /// so these stay `None` until the adapter's first fetch.
    connection: ObservedConnection,
    /// How a `429` is waited out. Exchange Online throttles a mailbox on both concurrency
    /// and a requests-per-window budget, and names its own wait in `Retry-After`.
    retry: RetryConfig,
}

impl HttpTransport {
    /// Builds a transport authenticating with an OAuth bearer access token.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`) and `retry` its
    /// throttling policy (`docs/agent-guidance/http-throttling.md`).
    pub(crate) fn new(
        token: String,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
    ) -> Result<Self, GraphError> {
        let retry = retry.clone().labelled("graph");
        // What Exchange Online allows one mailbox, stated once for the whole account: the
        // gate is the host's, shared by every provider it connected for this account, and
        // the number is the adapter's because only the adapter knows the server
        // (`engine_http::RequestGate`). A folder-bound provider that bounded itself would
        // be 56 ceilings of four.
        retry.gate().narrow_to(MAX_CONCURRENT_SOURCE_FETCHES);
        Ok(Self {
            client: tls.reqwest_builder().build()?,
            token,
            connection: ObservedConnection::default(),
            retry,
        })
    }

    /// Issues the authenticated, immutable-id-preferring `GET` the fetch shapes share,
    /// recording the negotiated HTTP and TLS versions on the way through — the one
    /// funnel, so no path can forget them. `extra_prefer` appends a second `Prefer` value
    /// (the calendar read's `outlook.timezone`).
    async fn send(
        &self,
        url: &str,
        extra_prefer: Option<&str>,
    ) -> Result<reqwest::Response, GraphError> {
        let prefer = match extra_prefer {
            Some(extra) => format!("IdType=\"ImmutableId\", {extra}"),
            None => "IdType=\"ImmutableId\"".to_owned(),
        };
        let response = send_retrying(
            self.client
                .get(url)
                .bearer_auth(&self.token)
                .header("Prefer", prefer),
            &self.retry,
        )
        .await?;
        self.connection.record(&response);
        Ok(response)
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
    ) -> Result<reqwest::Response, GraphError> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
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
        let response = send_retrying(request.body(body), &self.retry).await?;
        self.connection.record(&response);
        Ok(response)
    }

    /// Turns a byte response into its body, classifying a non-2xx. Shared by the
    /// authenticated and anonymous byte paths so they differ in exactly one thing:
    /// whether the bearer token is attached.
    async fn collect_bytes(resp: reqwest::Response) -> Result<Vec<u8>, GraphError> {
        let status = resp.status();
        if !status.is_success() {
            // The `$value` error body is JSON like any other Graph error, so classify it
            // the same way (an expired/moved message → the caller re-syncs and retries).
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.bytes().await?.to_vec())
    }
}

/// Turns a successful write response into its parsed JSON body, or `None` when the
/// action carried none (`202`/`204`). A non-2xx is a classified [`GraphError::Status`].
async fn write_body(resp: reqwest::Response) -> Result<Option<Value>, GraphError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(GraphError::status(status.as_u16(), body));
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
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.json::<Value>().await?)
    }

    async fn get_with_prefer(&self, url: &str, prefer: Option<&str>) -> Result<Value, GraphError> {
        let resp = self.send(url, prefer).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::status(status.as_u16(), body));
        }
        Ok(resp.json::<Value>().await?)
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        let resp = self.send(url, None).await?;
        Self::collect_bytes(resp).await
    }

    async fn get_bytes_unauthenticated(&self, url: &str) -> Result<Vec<u8>, GraphError> {
        // No `Authorization` and no `Prefer`: this URL came from payload content, not
        // from the Graph API, so it gets a bare GET.
        let resp = send_retrying(self.client.get(url), &self.retry).await?;
        self.connection.record(&resp);
        Self::collect_bytes(resp).await
    }

    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        let resp = self
            .send_write(reqwest::Method::POST, url, Some(content_type), None, body)
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
            )
            .await?;
        write_body(resp).await
    }

    async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GraphError> {
        let resp = self
            .send_write(reqwest::Method::DELETE, url, None, if_match, Vec::new())
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(GraphError::status(status.as_u16(), body))
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
