//! `provider-jmap` — the JMAP (RFC 8620/8621, RFC 8984) read/write provider.
//!
//! This crate is the first product protocol client. It talks JMAP over HTTP to a
//! server (the Stalwart test fixture in steps 4–5, real providers later),
//! discovers the session, ships batched method calls with result back-references,
//! and normalizes JMAP mail and calendar objects into the engine's
//! [`SyncUpdate`](engine_core::sync::SyncUpdate) shapes. It implements the
//! [`engine_provider::Provider`] contract so the sync orchestrator never switches
//! on provider kind.
//!
//! # Layers
//!
//! - `transport` — reqwest HTTP with auth and error mapping.
//! - `request` — the `{ using, methodCalls }` envelope, `#id` back-references, and typed response
//!   lookup.
//! - `session` — the session resource: capabilities, account ids, limits, and the
//!   [`SessionUrlPolicy`] for resolving advertised URLs.
//! - [`JmapClient`] — connect + execute, the low-level handle the normalization and `Provider` impl
//!   build on.
//!
//! # Two real-world notes
//!
//! - **Advertised origin ≠ connection origin.** Stalwart advertises `https://mail.test.local/` in
//!   its session while tests connect to `127.0.0.1:18080`; [`SessionUrlPolicy::RebaseToConnection`]
//!   (the default) keeps the path but forces the connection origin. Providers that genuinely serve
//!   their API cross-origin use [`SessionUrlPolicy::TrustAdvertised`].
//! - **Raw MIME is fetched on demand, not synced.** A normalized mail object keeps its JMAP
//!   `blobId`; `JmapProvider::fetch_message_source` downloads the raw RFC 5322 source through the
//!   session `downloadUrl` template (RFC 8620 §6.2) when a host opens the message. The sync itself
//!   still ships Tier-1 metadata only — durable raw-MIME storage at sync time awaits the store's
//!   blob sub-step. Calendar raw (`RawJsCalendar`) *is* preserved on the object
//!   (`docs/agent-guidance/jmap.md`).

mod auth;
mod calendar;
mod calendar_write;
mod error;
mod fetch;
mod json;
mod mail;
mod mutate;
mod provider;
mod request;
mod session;
mod submit;
mod submit_body;
mod sync_ops;
mod transport;
mod watch;

use core::fmt;
use std::sync::Arc;

use engine_provider::{ConnectObserver, ConnectStep, IgnoreConnectSteps};
use engine_tls::TlsClientConfig;
pub use error::JmapError;
pub use provider::JmapProvider;
use reqwest::Url;
pub use session::{CoreLimits, Session, SessionUrlPolicy};
pub use watch::{DEFAULT_EVENT_SOURCE_PING, JmapWatcher};

use crate::{
    request::{Request, Response},
    session::resolve_against,
    transport::Transport,
};

/// The maximum number of redirects followed while discovering the session
/// resource (the well-known endpoint 307-redirects to the session URL).
const MAX_SESSION_REDIRECTS: usize = 5;

/// Credentials for authenticating to a JMAP server.
///
/// This names the credential the caller *holds*, not the header that goes on the wire.
/// JMAP specifies no authentication mechanism of its own — RFC 8620 §8.2 defers to the
/// IANA scheme registry and marks Basic NOT RECOMMENDED — so the scheme is whatever the
/// server challenges for, and the transport re-frames the secret to match (see
/// `crate::auth`). A `Basic` credential is therefore also presentable as a bearer token;
/// the variant records that a username came with it, not that Basic will be sent.
///
/// `Debug` is redacted — the secret never appears in logs (`north-star.md` security).
#[derive(Clone)]
pub enum Credentials {
    /// A username and secret: a login password, an app-specific password, or an API
    /// token that the user happened to enter alongside their address.
    Basic {
        /// The username (full email address for the fixture).
        username: String,
        /// The password or app-specific token.
        password: String,
    },
    /// A bare secret with no username — an OAuth or API bearer token.
    Bearer(String),
}

impl Credentials {
    /// HTTP Basic credentials.
    #[must_use]
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Basic {
            username: username.into(),
            password: password.into(),
        }
    }

    /// An OAuth bearer token.
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Self {
        Self::Bearer(token.into())
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render the secret.
        let kind = match self {
            Self::Basic { username, .. } => format!("Basic {{ username: {username:?}, .. }}"),
            Self::Bearer(_) => "Bearer(..)".to_owned(),
        };
        f.write_str(&kind)
    }
}

/// How to connect a [`JmapClient`].
#[derive(Clone)]
pub struct JmapConfig {
    base_url: String,
    credentials: Credentials,
    session_path: String,
    session_urls: SessionUrlPolicy,
    tls: TlsClientConfig,
    connect_observer: Option<Arc<dyn ConnectObserver>>,
}

impl JmapConfig {
    /// Configures a connection to `base_url` (e.g. `http://127.0.0.1:18080`) with
    /// `credentials`, defaulting to well-known session discovery and rebasing
    /// advertised URLs onto the connection.
    #[must_use]
    pub fn new(base_url: impl Into<String>, credentials: Credentials) -> Self {
        Self {
            base_url: base_url.into(),
            credentials,
            session_path: "/.well-known/jmap".to_owned(),
            session_urls: SessionUrlPolicy::RebaseToConnection,
            tls: TlsClientConfig::default(),
            connect_observer: None,
        }
    }

    /// Overrides the session-discovery path (default `/.well-known/jmap`).
    #[must_use]
    pub fn with_session_path(mut self, path: impl Into<String>) -> Self {
        self.session_path = path.into();
        self
    }

    /// Overrides how advertised session URLs are resolved.
    #[must_use]
    pub fn with_session_urls(mut self, policy: SessionUrlPolicy) -> Self {
        self.session_urls = policy;
        self
    }

    /// Sets the TLS trust policy (the host builds one and shares it across the
    /// account's providers). Defaults to the hermetic bundled roots
    /// (`docs/agent-guidance/tls.md`).
    #[must_use]
    pub fn with_tls(mut self, tls: TlsClientConfig) -> Self {
        self.tls = tls;
        self
    }

    /// Observes the connect phase: one [`ConnectStep::Redirected`] per well-known hop,
    /// [`ConnectStep::Authenticated`] when the server serves the session, and
    /// [`ConnectStep::Discovered`] naming the `apiUrl` that will serve every method
    /// call. No TLS step — reqwest never exposes the negotiated version
    /// (`docs/agent-guidance/tls.md`).
    ///
    /// The observer rides on the config, so a host that rebuilds this client after a
    /// dropped session observes the redial too. `Arc` so one host observer can be
    /// shared across the account's providers.
    #[must_use]
    pub fn with_connect_observer(mut self, observer: Arc<dyn ConnectObserver>) -> Self {
        self.connect_observer = Some(observer);
        self
    }
}

impl fmt::Debug for JmapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JmapConfig")
            .field("base_url", &self.base_url)
            .field("session_path", &self.session_path)
            .field("session_urls", &self.session_urls)
            .finish_non_exhaustive()
    }
}

/// A connected JMAP client: an authenticated transport plus the resolved session.
///
/// Built with [`JmapClient::connect`], which fetches and resolves the session.
/// Method execution (`execute`, crate-internal) is what the mail and
/// calendar normalization build on.
pub struct JmapClient {
    transport: Transport,
    session: Session,
}

impl JmapClient {
    /// Connects to a JMAP server: builds the transport, discovers the session
    /// (following the well-known redirect, rebasing per the policy), and resolves
    /// capabilities, account ids, and limits.
    ///
    /// Reports each step to [`JmapConfig::with_connect_observer`]'s observer, if one
    /// is configured.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError`] on a bad base URL, a transport/HTTP failure, or a
    /// malformed/incomplete session resource.
    pub async fn connect(config: JmapConfig) -> Result<Self, JmapError> {
        let base = Url::parse(&config.base_url)
            .map_err(|e| JmapError::session(format!("bad base_url {:?}: {e}", config.base_url)))?;
        let observer: &dyn ConnectObserver = config
            .connect_observer
            .as_deref()
            .unwrap_or(&IgnoreConnectSteps);
        let transport = Transport::new(config.credentials, &config.tls)?;
        let document = fetch_session(
            &transport,
            &base,
            &config.session_path,
            config.session_urls,
            observer,
        )
        .await?;
        let session = Session::parse(&document, &base, config.session_urls)?;
        // The endpoint every method call will go to — the last thing connect resolves,
        // and (under `RebaseToConnection`) the one derived from the connection origin.
        observer.step(&ConnectStep::discovered(session.api_url()));
        Ok(Self { transport, session })
    }

    /// The resolved session (capabilities, account ids, limits, API URL).
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The HTTP version this client's connection negotiated — always populated once
    /// [`connect`](Self::connect) has fetched the session. The matching TLS version is
    /// not available: reqwest exposes only the peer certificate, never the negotiated
    /// protocol version (`docs/agent-guidance/tls.md`).
    pub(crate) fn http_version(&self) -> Option<engine_provider::HttpVersion> {
        self.transport.http_version()
    }

    /// Ships a batched request to the API endpoint and parses the response
    /// envelope. Method-level errors surface when a result is read
    /// ([`Response::result`]).
    ///
    /// # Errors
    ///
    /// Returns [`JmapError`] on a transport/HTTP failure or a malformed response.
    pub(crate) async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        let body = request.to_json();
        let value = self
            .transport
            .post_json(self.session.api_url(), &body)
            .await?;
        Response::parse(&value)
    }

    /// GETs raw bytes from an already-resolved blob-download `url` — the raw RFC
    /// 5322 source behind a message's `blobId` (RFC 8620 §6.2). The `url` is a
    /// fully-substituted download template (see `crate::fetch::message_source`).
    ///
    /// # Errors
    ///
    /// Returns [`JmapError`] on a transport/HTTP failure or a non-success status.
    pub(crate) async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        self.transport.get_bytes(url).await
    }

    /// Uploads raw `bytes` of `media_type` to an already-resolved blob-upload `url`
    /// (the session `uploadUrl` with `{accountId}` substituted), returning the
    /// server-assigned `blobId` (RFC 8620 §6.1) — a draft references it to attach a
    /// part (`crate::submit`).
    ///
    /// # Errors
    ///
    /// Returns [`JmapError`] on a transport/HTTP failure, a non-success status, or an
    /// upload response missing its `blobId`.
    pub(crate) async fn upload(
        &self,
        url: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<String, JmapError> {
        let value = self
            .transport
            .post_bytes(url, media_type, bytes.to_vec())
            .await?;
        value
            .get("blobId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| JmapError::protocol("upload response missing blobId"))
    }

    /// Opens the JMAP **EventSource** change-notification stream (RFC 8620 §7.3): a
    /// long-lived `text/event-stream` GET over the session `eventSourceUrl`, watching
    /// `types` (empty ⇒ all types, `*`), never closing early (`closeafter=no`), and
    /// asking the server to `ping` every `ping` seconds so the stream stays alive and
    /// surfaces keep-alives. Returns the streaming response for [`crate::watch`] to
    /// read chunk by chunk.
    ///
    /// # Errors
    ///
    /// [`JmapError::Session`] if the server advertised no `eventSourceUrl`, or the
    /// classified failure of opening the stream (a non-success status is
    /// [`JmapError::Status`]).
    pub(crate) async fn open_event_source(
        &self,
        types: &[&str],
        ping: core::time::Duration,
    ) -> Result<reqwest::Response, JmapError> {
        let template = self
            .session
            .event_source_url()
            .ok_or_else(|| JmapError::session("server advertised no eventSourceUrl"))?;
        let types_param = if types.is_empty() {
            "*".to_owned()
        } else {
            types.join(",")
        };
        // `ping=0` disables server pings (RFC 8620 §7.3); keep at least 1s so the
        // stream still emits keep-alives.
        let ping_secs = ping.as_secs().max(1);
        let url = template
            .replace("{types}", &types_param)
            .replace("{closeafter}", "no")
            .replace("{ping}", &ping_secs.to_string());
        // `Accept: text/event-stream` so a content-negotiating server serves the SSE
        // stream, not a buffered representation; the shared status check rejects a
        // non-2xx before the caller treats the body as an event stream.
        let resp = self.transport.get_event_stream(&url).await?;
        transport::error_for_status(resp).await
    }
}

impl fmt::Debug for JmapClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JmapClient")
            .field("api_url", &self.session.api_url())
            .finish_non_exhaustive()
    }
}

/// Fetches the session document, resolving the well-known redirect chain itself so
/// a foreign advertised origin can be rebased onto the connection. Reports one
/// [`ConnectStep::Redirected`] per hop and, once the server serves a success,
/// [`ConnectStep::Authenticated`].
async fn fetch_session(
    transport: &Transport,
    base: &Url,
    session_path: &str,
    policy: SessionUrlPolicy,
    observer: &dyn ConnectObserver,
) -> Result<serde_json::Value, JmapError> {
    let mut url = resolve_against(base, session_path, policy)?;
    for _ in 0..MAX_SESSION_REDIRECTS {
        let resp = transport.get(&url).await?;
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| JmapError::session("redirect without Location"))?;
            let next = resolve_against(base, location, policy)?;
            // Both sides resolved, so a host sees the hop it can actually replay —
            // not a bare `Location` path whose origin it would have to reconstruct.
            observer.step(&ConnectStep::redirected(&url, &next));
            url = next;
            continue;
        }
        // The request carried the account's credentials, so a non-redirect success is
        // the server accepting them. A 401/403 becomes a `JmapError` below instead.
        if status.is_success() {
            observer.step(&ConnectStep::Authenticated);
        }
        return transport::read_json(resp).await;
    }
    Err(JmapError::session("too many session redirects"))
}

/// Fuzzing entry point: run untrusted bytes through the JMAP JSON parse +
/// normalize pipeline, discarding results.
///
/// Mail and calendar payloads are hostile input; the parsers must never panic on
/// it (`north-star.md` security). Behind the `fuzzing` feature so it is not part
/// of the normal public API; the `fuzz/` cargo-fuzz target drives it (run with
/// `cargo +nightly fuzz run jmap_parse`).
#[cfg(feature = "fuzzing")]
pub fn fuzz_parse(data: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let _ = mail::mailbox_from_json(&value);
    let _ = mail::message_from_json(&value);
    let _ = calendar::calendar_from_json(&value);
    let _ = calendar::event_from_json(&value);
    let _ = request::Response::parse(&value);
    let _ = sync_ops::Changes::parse(&value);
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
