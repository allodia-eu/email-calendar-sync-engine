//! reqwest-backed HTTP transport for JMAP.
//!
//! Thin wrapper that applies authentication, ships the JSON envelope, and maps
//! HTTP/transport failures into [`JmapError`]. Redirects are **not** auto-followed:
//! the session-discovery flow in [`crate`] resolves the well-known redirect itself
//! so it can rebase a foreign advertised origin onto the connection (see
//! [`SessionUrlPolicy`](crate::SessionUrlPolicy)).
//!
//! Every request asks the account's [`CredentialSource`] for the credential it carries,
//! through one `engine_http::AuthenticatedClient`: session discovery and each redirect hop,
//! method calls, blob downloads and uploads, and the push stream. So no path can hold a stale
//! token, skip the plaintext rule or the source's `permits`, or replay a refused request twice.

use std::sync::Arc;

use engine_http::{
    AuthenticatedClient, Exchange, ObservedConnection, RetryConfig, Sent, Unauthorized,
};
use engine_provider::{Credential, CredentialFailure, CredentialSource, HttpVersion, TlsVersion};
use engine_tls::TlsClientConfig;
use reqwest::{RequestBuilder, Url};
use serde_json::Value;

use crate::{
    auth::{AuthScheme, Challenge, NegotiatedScheme, read_challenge},
    error::JmapError,
};

/// An authenticated HTTP transport.
pub(crate) struct Transport {
    /// The account's client for requests that carry its credential: its configured origin, its
    /// credential source, how a `429` is waited out, and a `reqwest` client that follows no
    /// redirect. A JMAP server also has protocol-level `rateLimit` / `overQuota` errors inside
    /// a `200`, which the method layer classifies; the client's throttling covers only the
    /// HTTP one, which is the shape a blob download meets.
    http: AuthenticatedClient,
    /// The scheme credentials are presented under once a server has said which. JMAP
    /// specifies no authentication mechanism of its own (RFC 8620 §8.2) — the server declares
    /// one in its `401` challenge — so a credential starts at its natural scheme and moves if
    /// a server says otherwise. See [`crate::auth`].
    scheme: NegotiatedScheme,
    /// The HTTP and TLS versions most recently observed — the post-connect facts
    /// `ConnectionInfo` reports. Every request funnels through
    /// [`Transport::send`], and [`JmapClient::connect`](crate::JmapClient::connect)
    /// fetches the session, so this is populated by the time a client exists. It then
    /// keeps tracking: the well-known redirect this transport follows *itself* may be a
    /// different origin from the `apiUrl` that serves method calls, so the latest
    /// observation — not the first — is the one that describes the working connection.
    connection: ObservedConnection,
}

impl Transport {
    /// Builds a transport for the account configured at `configured`, trusting per `tls`
    /// (`docs/agent-guidance/tls.md`), presenting what `source` answers. Its client follows no
    /// redirect: session discovery follows the well-known one itself, as a request of its own.
    pub(crate) fn new(
        configured: &Url,
        source: Arc<dyn CredentialSource>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, JmapError> {
        let retry = retry
            .clone()
            .labelled("jmap")
            // A concurrency refusal arrives as a `400`, which the shared status rule
            // reads as a request defect rather than as "not yet". See `JmapThrottles`.
            .classifying(Arc::new(crate::error::JmapThrottles));
        Ok(Self {
            http: AuthenticatedClient::new(tls, user_agent, configured.clone(), source, retry)?,
            scheme: NegotiatedScheme::default(),
            connection: ObservedConnection::default(),
        })
    }

    /// The HTTP version negotiated on this transport's connection, or `None` before
    /// its first response.
    pub(crate) fn http_version(&self) -> Option<HttpVersion> {
        self.connection.http_version()
    }

    /// The TLS version negotiated on this transport's connection, or `None` before its
    /// first response over TLS.
    pub(crate) fn tls_version(&self) -> Option<TlsVersion> {
        self.connection.tls_version()
    }

    /// Frames `credential` on `builder` under the scheme it is presented under now, and
    /// records that scheme in `framing`, where the challenge of a `401` is read against it.
    ///
    /// A re-framing `judge` proposed is latched here, when the funnel presents the credential
    /// again, and only then: a proposal the funnel does not act on (a second re-framing, a body
    /// that cannot be sent again) leaves the latch as it was.
    fn present(
        &self,
        builder: RequestBuilder,
        credential: &Credential,
        framing: &Framing,
    ) -> Result<RequestBuilder, JmapError> {
        if let Some(next) = framing.proposed.take() {
            self.scheme.set(next);
        }
        let scheme = self.scheme.scheme_for(credential);
        if let Some(scheme) = scheme {
            framing.used.set(scheme);
        }
        match (credential, scheme) {
            (Credential::Password { username, password }, Some(AuthScheme::Basic)) => Ok(
                engine_http::present_basic(builder, username, password.expose())?,
            ),
            // The same secret framed bare, where a server asked for bearer tokens.
            (Credential::Password { password, .. }, Some(AuthScheme::Bearer)) => {
                Ok(engine_http::present_token(builder, password.expose())?)
            }
            (Credential::Bearer { token, .. }, Some(_)) => {
                Ok(engine_http::present_token(builder, token.expose())?)
            }
            _ => Err(CredentialFailure::unpresentable(
                "this kind of credential cannot be presented over JMAP",
            )
            .into()),
        }
    }

    /// Reads a `401` to a request that carried `credential` as `framing` records: a challenge
    /// for another scheme the credential can be framed under proposes it, and asks the funnel to
    /// present the same credential again; one for the scheme used, or none, refuses the
    /// credential; one for schemes it cannot be framed under at all is final ([`crate::auth`]).
    fn judge(reply: &Sent, credential: &Credential, framing: &Framing) -> Unauthorized {
        // Read as every adapter reads them (`engine_http::offered_schemes`): a challenge whose
        // realm is not ASCII still offers its scheme. Each scheme reads back as itself.
        let offered = engine_http::offered_schemes(reply.headers());
        let challenges = offered.iter().map(String::as_str);
        // `present` always records a scheme before anything is sent.
        let used = framing.used.latched().unwrap_or(AuthScheme::Basic);
        match read_challenge(used, challenges, credential) {
            Challenge::Reframe(next) => {
                framing.proposed.set(next);
                Unauthorized::Reframe
            }
            Challenge::Refused => Unauthorized::Refused,
            Challenge::Unusable => Unauthorized::Final,
        }
    }

    /// Authenticates and ships `builder` (`engine_http::AuthenticatedClient::send`), honouring the
    /// server's challenge: a `401` whose `WWW-Authenticate` does not offer the scheme used is
    /// replayed once under a scheme it does offer, which is then latched (RFC 9110 §11.6.1;
    /// see [`crate::auth`]). A `401` that *does* offer it refuses the credential, and the
    /// account's source is asked once for a replacement; the request goes once more if it has
    /// one. A `401` that survives both comes back for the caller to read.
    ///
    /// This is the one funnel every request in this transport passes, so no path can
    /// forget the credential rules, the version observation or the negotiation. The engine's
    /// shared client offers ALPN `h2` then `http/1.1` (`docs/agent-guidance/tls.md`), so this
    /// is HTTP/2 wherever the server supports it.
    async fn send(
        &self,
        builder: RequestBuilder,
        exchange: Exchange<'_>,
    ) -> Result<Sent, JmapError> {
        let framing = Framing::default();
        let response = self
            .http
            .send(
                builder,
                exchange,
                |builder, credential| self.present(builder, credential, &framing),
                |reply, credential| Self::judge(reply, credential, &framing),
            )
            .await?;
        self.connection.record(&response);
        Ok(response)
    }

    /// Sends an authenticated GET, returning the raw response so the caller can
    /// inspect a redirect's status and `Location` before reading any body.
    pub(crate) async fn get(&self, url: &str) -> Result<Sent, JmapError> {
        self.send(self.http.get(url), Exchange::Ordinary).await
    }

    /// Opens an authenticated GET declaring `Accept: text/event-stream` — the JMAP
    /// EventSource push stream (RFC 8620 §7.3). The `Accept` header is what a
    /// content-negotiating server/proxy keys on to serve the streaming SSE
    /// representation rather than a buffered JSON/HTML one. The status is **not**
    /// checked here; the caller does so before treating the body as a stream.
    pub(crate) async fn get_event_stream(&self, url: &str) -> Result<Sent, JmapError> {
        self.send(
            self.http
                .get(url)
                .header(reqwest::header::ACCEPT, "text/event-stream"),
            Exchange::Ordinary,
        )
        .await
    }

    /// POSTs `body` as JSON and parses a success response as a JSON value. `exchange` says
    /// whether the request submits a message, which decides how long its reply may take.
    pub(crate) async fn post_json(
        &self,
        url: &str,
        body: &Value,
        exchange: Exchange<'_>,
    ) -> Result<Value, JmapError> {
        let resp = self.send(self.http.post(url).json(body), exchange).await?;
        read_json(resp).await
    }

    /// GETs `url` and returns the raw response body bytes — the blob-download path
    /// for a message's raw RFC 5322 source (RFC 8620 §6.2). Maps a non-success
    /// status to [`JmapError::Status`] via [`error_for_status`].
    pub(crate) async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        let resp = self.send(self.http.get(url), Exchange::Ordinary).await?;
        Ok(error_for_status(resp).await?.bytes().await?)
    }

    /// GETs `url` and returns the reply with its body unread, once its status says it is a
    /// success: a blob read a piece at a time (`crate::blob::BlobDownload`).
    pub(crate) async fn get_streaming(&self, url: &str) -> Result<reqwest::Response, JmapError> {
        let resp = self.send(self.http.get(url), Exchange::Ordinary).await?;
        Ok(error_for_status(resp).await?.into_streaming())
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
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, content_type.to_owned())
                    .body(bytes),
                Exchange::Ordinary,
            )
            .await?;
        read_json(resp).await
    }
}

/// Reads a JSON body, mapping a non-success status to [`JmapError::Status`] with
/// the body captured for diagnostics.
pub(crate) async fn read_json(resp: engine_http::Sent) -> Result<Value, JmapError> {
    let body = error_for_status(resp).await?.bytes().await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Returns `resp` unchanged on a success (2xx) status, else consumes its body into a
/// [`JmapError::Status`] carrying the code + body for diagnostics. The single place
/// that turns an HTTP error status into an engine error, shared by the JSON, blob, and
/// EventSource paths so their failure handling cannot drift.
pub(crate) async fn error_for_status(resp: engine_http::Sent) -> Result<Sent, JmapError> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        // Read before the body consumes the reply. Usually `None` here — JMAP's own
        // refusals name a limit and not an instant — but a server may still send a
        // `Retry-After`, and this is the last place it exists.
        let wait = resp.stated_wait();
        let body = resp.text().await.unwrap_or_default();
        Err(JmapError::status(status.as_u16(), body).with_retry_after(wait))
    }
}

/// How one request framed its credential: the scheme it went out under, and a re-framing
/// `judge` proposed that the next presentation latches.
#[derive(Debug, Default)]
struct Framing {
    used: NegotiatedScheme,
    proposed: NegotiatedScheme,
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;

#[cfg(test)]
#[path = "credential_tests.rs"]
mod credential_tests;

#[cfg(test)]
#[path = "credential_failure_tests.rs"]
mod credential_failure_tests;

#[cfg(test)]
#[path = "credential_origin_tests.rs"]
mod credential_origin_tests;

#[cfg(test)]
#[path = "credential_refusal_tests.rs"]
mod credential_refusal_tests;

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod deadline_tests;
