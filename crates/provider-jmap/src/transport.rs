//! reqwest-backed HTTP transport for JMAP.
//!
//! Thin wrapper that applies authentication, ships the JSON envelope, and maps
//! HTTP/transport failures into [`JmapError`]. Redirects are **not** auto-followed:
//! the session-discovery flow in [`crate`] resolves the well-known redirect itself
//! so it can rebase a foreign advertised origin onto the connection (see
//! [`SessionUrlPolicy`](crate::SessionUrlPolicy)).

use engine_tls::TlsClientConfig;
use reqwest::{Client, RequestBuilder, redirect::Policy};
use serde_json::Value;

use crate::{Credentials, error::JmapError};

/// An authenticated HTTP transport.
pub(crate) struct Transport {
    client: Client,
    credentials: Credentials,
}

impl Transport {
    /// Builds a transport with redirect-following disabled, trusting per `tls`
    /// (`docs/agent-guidance/tls.md`).
    pub(crate) fn new(credentials: Credentials, tls: &TlsClientConfig) -> Result<Self, JmapError> {
        let client = tls.reqwest_builder().redirect(Policy::none()).build()?;
        Ok(Self {
            client,
            credentials,
        })
    }

    /// Applies the configured credentials to a request builder.
    fn authed(&self, builder: RequestBuilder) -> RequestBuilder {
        match &self.credentials {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer(token) => builder.bearer_auth(token),
        }
    }

    /// Sends an authenticated GET, returning the raw response so the caller can
    /// inspect a redirect's status and `Location` before reading any body.
    pub(crate) async fn get(&self, url: &str) -> Result<reqwest::Response, JmapError> {
        Ok(self.authed(self.client.get(url)).send().await?)
    }

    /// Opens an authenticated GET declaring `Accept: text/event-stream` — the JMAP
    /// EventSource push stream (RFC 8620 §7.3). The `Accept` header is what a
    /// content-negotiating server/proxy keys on to serve the streaming SSE
    /// representation rather than a buffered JSON/HTML one. The status is **not**
    /// checked here; the caller does so before treating the body as a stream.
    pub(crate) async fn get_event_stream(&self, url: &str) -> Result<reqwest::Response, JmapError> {
        Ok(self
            .authed(self.client.get(url))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await?)
    }

    /// POSTs `body` as JSON and parses a success response as a JSON value.
    pub(crate) async fn post_json(&self, url: &str, body: &Value) -> Result<Value, JmapError> {
        let resp = self.authed(self.client.post(url)).json(body).send().await?;
        read_json(resp).await
    }

    /// GETs `url` and returns the raw response body bytes — the blob-download path
    /// for a message's raw RFC 5322 source (RFC 8620 §6.2). Maps a non-success
    /// status to [`JmapError::Status`] via [`error_for_status`].
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        let resp = error_for_status(self.authed(self.client.get(url)).send().await?).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// POSTs raw `bytes` with `Content-Type: content_type` and parses the JSON
    /// response — the blob-upload path (RFC 8620 §6.1), which returns a
    /// `{ accountId, blobId, type, size }` object naming the stored blob.
    pub(crate) async fn post_bytes(
        &self,
        url: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<Value, JmapError> {
        let resp = self
            .authed(self.client.post(url))
            .header(reqwest::header::CONTENT_TYPE, content_type.to_owned())
            .body(bytes)
            .send()
            .await?;
        read_json(resp).await
    }
}

/// Reads a JSON body, mapping a non-success status to [`JmapError::Status`] with
/// the body captured for diagnostics.
pub(crate) async fn read_json(resp: reqwest::Response) -> Result<Value, JmapError> {
    Ok(error_for_status(resp).await?.json::<Value>().await?)
}

/// Returns `resp` unchanged on a success (2xx) status, else consumes its body into a
/// [`JmapError::Status`] carrying the code + body for diagnostics. The single place
/// that turns an HTTP error status into an engine error, shared by the JSON, blob, and
/// EventSource paths so their failure handling cannot drift.
pub(crate) async fn error_for_status(
    resp: reqwest::Response,
) -> Result<reqwest::Response, JmapError> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(JmapError::status(status.as_u16(), body))
    }
}
