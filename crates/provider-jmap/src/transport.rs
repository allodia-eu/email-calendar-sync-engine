//! reqwest-backed HTTP transport for JMAP.
//!
//! Thin wrapper that applies authentication, ships the JSON envelope, and maps
//! HTTP/transport failures into [`JmapError`]. Redirects are **not** auto-followed:
//! the session-discovery flow in [`crate`] resolves the well-known redirect itself
//! so it can rebase a foreign advertised origin onto the connection (see
//! [`SessionUrlPolicy`](crate::SessionUrlPolicy)).

use engine_provider::{HttpVersion, ObservedHttpVersion};
use engine_tls::TlsClientConfig;
use reqwest::{Client, RequestBuilder, StatusCode, header::WWW_AUTHENTICATE, redirect::Policy};
use serde_json::Value;

use crate::{
    Credentials,
    auth::{AuthScheme, NegotiatedScheme, negotiate},
    error::JmapError,
};

/// An authenticated HTTP transport.
pub(crate) struct Transport {
    client: Client,
    credentials: Credentials,
    /// The scheme credentials are currently presented under. JMAP specifies no
    /// authentication mechanism of its own (RFC 8620 §8.2) — the server declares one in
    /// its `401` challenge — so this starts at the credential's natural scheme and
    /// moves if a server says otherwise. See [`crate::auth`].
    scheme: NegotiatedScheme,
    /// The HTTP version most recently observed — the post-connect fact
    /// `ConnectionInfo::http_version` reports. Every request funnels through
    /// [`Transport::send`], and [`JmapClient::connect`](crate::JmapClient::connect)
    /// fetches the session, so this is populated by the time a client exists. It then
    /// keeps tracking: the well-known redirect this transport follows *itself* may be a
    /// different origin from the `apiUrl` that serves method calls, so the latest
    /// observation — not the first — is the one that describes the working connection.
    http_version: ObservedHttpVersion,
}

impl Transport {
    /// Builds a transport with redirect-following disabled, trusting per `tls`
    /// (`docs/agent-guidance/tls.md`).
    pub(crate) fn new(credentials: Credentials, tls: &TlsClientConfig) -> Result<Self, JmapError> {
        let client = tls.reqwest_builder().redirect(Policy::none()).build()?;
        Ok(Self {
            client,
            scheme: NegotiatedScheme::new(credentials.preferred_scheme()),
            credentials,
            http_version: ObservedHttpVersion::default(),
        })
    }

    /// The HTTP version negotiated on this transport's connection, or `None` before
    /// its first response.
    pub(crate) fn http_version(&self) -> Option<HttpVersion> {
        self.http_version.get()
    }

    /// Applies the configured credentials to a request builder under `scheme`.
    fn authed(&self, builder: RequestBuilder, scheme: AuthScheme) -> RequestBuilder {
        match (&self.credentials, scheme) {
            (Credentials::Basic { username, password }, AuthScheme::Basic) => {
                builder.basic_auth(username, Some(password))
            }
            // Every other pairing presents the bare secret as a bearer token. A bearer
            // credential is never asked to produce a Basic header — it has no username,
            // and `Credentials::can_present` refuses that switch — so this arm is only
            // ever the bearer framing of whichever secret we hold.
            _ => builder.bearer_auth(self.credentials.bearer_secret()),
        }
    }

    /// Authenticates and ships `builder`, honouring the server's authentication
    /// challenge: a `401` whose `WWW-Authenticate` does not offer the scheme we used is
    /// replayed once under a scheme it does offer, which is then latched for the rest of
    /// the connection (RFC 9110 §11.6.1; see [`crate::auth`]). A `401` that *does* offer
    /// our scheme is returned untouched — that is a wrong credential, not a wrong
    /// framing.
    ///
    /// This is the one funnel every request in this transport passes, so no path can
    /// forget either the version observation or the negotiation.
    async fn send(&self, builder: RequestBuilder) -> Result<reqwest::Response, JmapError> {
        let scheme = self.scheme.get();
        // Cloned before the body is consumed, so a scheme switch replays the identical
        // request. Only a streaming body would refuse to clone, and this transport sends
        // none — every body is a JSON value or an owned byte vector.
        let replay = builder.try_clone();
        let response = self.dispatch(self.authed(builder, scheme)).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let next = {
            let challenges = response
                .headers()
                .get_all(WWW_AUTHENTICATE)
                .iter()
                .filter_map(|value| value.to_str().ok());
            negotiate(scheme, challenges, &self.credentials)
        };
        let (Some(next), Some(replay)) = (next, replay) else {
            return Ok(response);
        };
        self.scheme.set(next);
        self.dispatch(self.authed(replay, next)).await
    }

    /// Ships a fully authenticated `builder`, recording the negotiated HTTP version on
    /// the way through. The engine's shared client offers ALPN `h2` then `http/1.1`
    /// (`docs/agent-guidance/tls.md`), so this is HTTP/2 wherever the server supports it.
    async fn dispatch(&self, builder: RequestBuilder) -> Result<reqwest::Response, JmapError> {
        let response = builder.send().await?;
        self.http_version.record(response.version());
        Ok(response)
    }

    /// Sends an authenticated GET, returning the raw response so the caller can
    /// inspect a redirect's status and `Location` before reading any body.
    pub(crate) async fn get(&self, url: &str) -> Result<reqwest::Response, JmapError> {
        self.send(self.client.get(url)).await
    }

    /// Opens an authenticated GET declaring `Accept: text/event-stream` — the JMAP
    /// EventSource push stream (RFC 8620 §7.3). The `Accept` header is what a
    /// content-negotiating server/proxy keys on to serve the streaming SSE
    /// representation rather than a buffered JSON/HTML one. The status is **not**
    /// checked here; the caller does so before treating the body as a stream.
    pub(crate) async fn get_event_stream(&self, url: &str) -> Result<reqwest::Response, JmapError> {
        self.send(
            self.client
                .get(url)
                .header(reqwest::header::ACCEPT, "text/event-stream"),
        )
        .await
    }

    /// POSTs `body` as JSON and parses a success response as a JSON value.
    pub(crate) async fn post_json(&self, url: &str, body: &Value) -> Result<Value, JmapError> {
        let resp = self.send(self.client.post(url).json(body)).await?;
        read_json(resp).await
    }

    /// GETs `url` and returns the raw response body bytes — the blob-download path
    /// for a message's raw RFC 5322 source (RFC 8620 §6.2). Maps a non-success
    /// status to [`JmapError::Status`] via [`error_for_status`].
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        let resp = self.send(self.client.get(url)).await?;
        Ok(error_for_status(resp).await?.bytes().await?.to_vec())
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
            .send(
                self.client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, content_type.to_owned())
                    .body(bytes),
            )
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

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;
