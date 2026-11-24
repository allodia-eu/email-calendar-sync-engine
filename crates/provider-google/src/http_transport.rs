//! The production reqwest [`GoogleTransport`]: bearer auth over the standard HTTP
//! stack.
//!
//! The one funnel every request flows through records the negotiated HTTP and TLS
//! versions, so no path forgets them. Google needs no request-shaping preference header
//! (unlike Graph's immutable-id `Prefer`): reads are plain bearer `GET`s. The write
//! verbs (a shared `POST`/`PATCH`/`DELETE` funnel) are added by the write slices as
//! they land. The offline tests drive it over a blocking single-shot mock server (no
//! network).

use async_trait::async_trait;
use engine_http::{Exchange, ObservedConnection, RetryConfig, send_retrying};
use engine_provider::{HttpVersion, TlsVersion};
use engine_tls::TlsClientConfig;
use serde_json::Value;

use crate::{error::GoogleError, transport::GoogleTransport};

/// The production reqwest transport: bearer auth.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    token: String,
    /// The HTTP and TLS versions most recently observed. `GoogleClient::connect`
    /// performs no request (Google has no session-discovery step), so these stay `None`
    /// until the adapter's first fetch.
    connection: ObservedConnection,
    /// How a throttle is waited out — including the one only a body can recognise. Gmail
    /// refuses in two shapes: a `429` past its concurrency ceiling, which the shared status
    /// rule handles, and a `403` when the per-minute quota runs out, which is the one an
    /// ordinary pass meets and the one `crate::throttle` exists for.
    retry: RetryConfig,
}

impl HttpTransport {
    /// Builds a transport authenticating with an OAuth bearer access token.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError::Transport`] if the HTTP client cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`).
    pub(crate) fn new(
        token: String,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, GoogleError> {
        let retry = retry
            .clone()
            .labelled("gmail")
            // Gmail's per-minute quota refusal is a `403`, which the shared status rule
            // reads as "not allowed" rather than "not yet". See `throttle.rs`.
            .classifying(std::sync::Arc::new(crate::throttle::GoogleThrottles));
        // The adapter's own fan-out width, applied account-wide — **not** Gmail's ceiling,
        // and the distinction matters.
        //
        // Gmail does have a concurrency ceiling, measured as salvos with a full quota window's
        // rest between widths: clean through 48, refusing from 64 upward with
        // `429 "Too many concurrent requests for user."`, the share climbing with width. That
        // is two to three times this number, so nothing the adapter does goes near it. What it
        // *does* meet is the rate — Total Query Cost units per minute per user, announced as
        // `403 rateLimitExceeded` — and a width bound does not control a rate
        // (`docs/agent-guidance/http-throttling.md`).
        //
        // So narrowing here is for the reason the gate exists rather than for the reason Graph
        // narrows: an account's mail, calendar and contacts providers would otherwise each run
        // their own `MAX_CONCURRENT_GETS`-wide fan-out against one user's quota, and three
        // times the width drains it three times as fast.
        retry.gate().narrow_to(crate::fetch::MAX_CONCURRENT_GETS);
        // The limit an ordinary pass actually meets is the rate, so the rate is bounded too.
        crate::quota::pace(retry.gate());
        Ok(Self {
            client: engine_http::client(tls, user_agent).build()?,
            token,
            connection: ObservedConnection::default(),
            retry,
        })
    }

    /// Issues an authenticated write (`POST`/`PATCH`/`DELETE`) the write shapes share,
    /// recording the negotiated HTTP and TLS versions — the write funnel, so every path
    /// observes them. Carries an optional `Content-Type` and `If-Match` precondition.
    async fn send_write(
        &self,
        method: reqwest::Method,
        url: &str,
        content_type: Option<&str>,
        if_match: Option<&str>,
        body: Vec<u8>,
        exchange: Exchange<'_>,
    ) -> Result<engine_http::Sent, GoogleError> {
        crate::quota::charge(self.retry.gate(), &method, url).await;
        let mut request = self.client.request(method, url).bearer_auth(&self.token);
        if let Some(content_type) = content_type {
            request = request.header("Content-Type", content_type);
        }
        if let Some(if_match) = if_match {
            request = request.header("If-Match", if_match);
        }
        let response = send_retrying(request.body(body), &self.retry, exchange).await?;
        self.connection.record(&response);
        Ok(response)
    }

    /// Sends a prepared byte fetch, recording the negotiated versions and classifying a
    /// non-2xx. Shared by the authenticated and anonymous byte paths so they differ in
    /// exactly one thing: whether the bearer token is attached.
    async fn fetch_bytes(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>, GoogleError> {
        let resp = send_retrying(request, &self.retry, Exchange::Ordinary).await?;
        self.connection.record(&resp);
        let status = resp.status();
        if !status.is_success() {
            let wait = resp.stated_wait();
            let body = resp.text().await.unwrap_or_default();
            return Err(GoogleError::status(status.as_u16(), body).with_retry_after(wait));
        }
        Ok(resp.bytes().await?)
    }
}

/// Turns a successful write response into its parsed JSON body, or `None` when the
/// action carried none (`204`). A non-2xx is a classified [`GoogleError::Status`].
async fn write_body(resp: engine_http::Sent) -> Result<Option<Value>, GoogleError> {
    let status = resp.status();
    if !status.is_success() {
        let wait = resp.stated_wait();
        let body = resp.text().await.unwrap_or_default();
        return Err(GoogleError::status(status.as_u16(), body).with_retry_after(wait));
    }
    let text = resp.text().await.unwrap_or_default();
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&text)?))
    }
}

#[async_trait]
impl GoogleTransport for HttpTransport {
    async fn get(&self, url: &str) -> Result<Value, GoogleError> {
        crate::quota::charge(self.retry.gate(), &reqwest::Method::GET, url).await;
        let request = self.client.get(url).bearer_auth(&self.token);
        let resp = send_retrying(request, &self.retry, Exchange::Ordinary).await?;
        self.connection.record(&resp);
        let status = resp.status();
        if !status.is_success() {
            let wait = resp.stated_wait();
            let body = resp.text().await.unwrap_or_default();
            return Err(GoogleError::status(status.as_u16(), body).with_retry_after(wait));
        }
        let bytes = resp.bytes().await?;
        if status == reqwest::StatusCode::NO_CONTENT && bytes.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GoogleError> {
        crate::quota::charge(self.retry.gate(), &reqwest::Method::GET, url).await;
        self.fetch_bytes(self.client.get(url).bearer_auth(&self.token))
            .await
    }

    async fn get_bytes_unauthenticated(&self, url: &str) -> Result<Vec<u8>, GoogleError> {
        self.fetch_bytes(self.client.get(url)).await
    }

    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
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
    ) -> Result<Option<Value>, GoogleError> {
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

    async fn put(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        let resp = self
            .send_write(
                reqwest::Method::PUT,
                url,
                Some(content_type),
                None,
                body,
                Exchange::Ordinary,
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
    ) -> Result<Option<Value>, GoogleError> {
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

    async fn delete(&self, url: &str, if_match: Option<&str>) -> Result<(), GoogleError> {
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
            Err(GoogleError::status(status.as_u16(), body).with_retry_after(wait))
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
