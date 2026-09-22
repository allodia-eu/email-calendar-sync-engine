//! `provider-jmap` — the JMAP (RFC 8620/8621, RFC 8984) read/write provider.
//!
//! This crate is the first product protocol client. It talks JMAP over HTTP to a
//! server (the Stalwart test fixture in steps 4–5, real providers later),
//! discovers the session, ships batched method calls with result back-references,
//! and normalizes JMAP mail, calendar, and contact objects into the engine's
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
mod blob;
mod calendar;
mod calendar_patch;
mod calendar_rsvp;
mod calendar_rule;
mod calendar_write;
mod config;
mod contact;
mod contact_fields;
mod contact_write;
mod contact_write_fields;
mod drafts;
mod drafts_blobs;
mod error;
mod executor;
mod fetch;
mod identity;
mod json;
mod mail;
mod mutate;
mod provider;
mod provider_calendar;
mod report;
mod request;
mod session;
mod session_accounts;
mod session_urls;
mod shared;
mod submit;
mod submit_body;
mod sync_ops;
mod transport;
mod watch;

use core::fmt;

pub use config::{Credentials, JmapConfig};
use engine_provider::{ConnectObserver, ConnectStep, IgnoreConnectSteps};
pub use error::JmapError;
pub use provider::JmapProvider;
use reqwest::Url;
pub use session::{CoreLimits, Session, SessionUrlPolicy};
pub use watch::{DEFAULT_EVENT_SOURCE_PING, JmapWatcher};

use crate::{
    request::{Request, Response},
    session_urls::resolve_against,
    transport::Transport,
};

/// The maximum number of redirects followed while discovering the session
/// resource (the well-known endpoint 307-redirects to the session URL).
const MAX_SESSION_REDIRECTS: usize = 5;

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
        let transport = Transport::new(config.credentials, &config.tls, &config.retry)?;
        let (document, served_by) = fetch_session(
            &transport,
            &base,
            &config.session_path,
            config.session_urls,
            observer,
        )
        .await?;
        // The session resolves against the URL that served it, not the URL discovery
        // started from. A chain that changed origin means the advertised `apiUrl`
        // belongs to the new one, and rebasing it onto the old one aims every method
        // call at a host that never had the session.
        let session = Session::parse(
            &document,
            &served_by,
            config.session_urls,
            config.shared_mailbox.as_ref(),
        )?;
        // What this server said it allows, stated once for the whole account. RFC 8620 §2
        // scopes `maxConcurrentRequests` to the API endpoint and defines no companion for
        // downloads, but a server may apply one number to both and Stalwart does — so it
        // bounds a body warm too (`docs/agent-guidance/http-throttling.md`). Known only
        // here, since the session is where the server states it.
        config
            .retry
            .gate()
            .narrow_to(session.limits().max_concurrent_requests);
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
    /// [`connect`](Self::connect) has fetched the session.
    pub(crate) fn http_version(&self) -> Option<engine_provider::HttpVersion> {
        self.transport.http_version()
    }

    /// The TLS version this client's connection negotiated, or `None` against a
    /// plaintext `http://` endpoint (`docs/agent-guidance/tls.md`). Populated from the
    /// same response as the HTTP version, so it too is known once
    /// [`connect`](Self::connect) has fetched the session.
    pub(crate) fn tls_version(&self) -> Option<engine_provider::TlsVersion> {
        self.transport.tls_version()
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
        // The status is checked on the reply, and only then is the body taken out unread:
        // an SSE stream has no end, so it is the one body nothing here may read.
        transport::error_for_status(resp)
            .await
            .map(engine_http::Sent::into_streaming)
    }
}

impl fmt::Debug for JmapClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JmapClient")
            .field("api_url", &self.session.api_url())
            .finish_non_exhaustive()
    }
}

/// Fetches the session document, following the well-known redirect chain itself.
/// Reports one [`ConnectStep::Redirected`] per hop and, once the server serves a
/// success, [`ConnectStep::Authenticated`].
///
/// Returns the document together with the URL that served it, which becomes the base
/// the session's advertised URLs resolve against. The two can name different origins:
/// RFC 8620 §2.2 discovery starts at the user's domain, and a hosted provider routinely
/// redirects that to the host actually running the server.
///
/// [`SessionUrlPolicy`] deliberately plays no part in a hop. It governs what a server
/// *advertises about itself* in a document it generated, where a public hostname it is
/// not reached at is a known misconfiguration worth correcting. A `Location` is not
/// that: it is the server saying where the resource is, resolved against the URL that
/// issued it per RFC 9110 §10.2.2. Rebasing one onto the connection origin discards the
/// move and re-requests the URL it came from, which reads as a redirect loop.
async fn fetch_session(
    transport: &Transport,
    base: &Url,
    session_path: &str,
    policy: SessionUrlPolicy,
    observer: &dyn ConnectObserver,
) -> Result<(serde_json::Value, Url), JmapError> {
    // The starting URL *is* policy-resolved: `session_path` is our own configuration,
    // not something a server said, and an override pointing at a public hostname needs
    // the same rebase every other configured URL gets.
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
            let next = engine_provider::redirect_target(&url, location).ok_or_else(|| {
                // Unparseable, or a hop off TLS — which these requests may never take,
                // since every one of them carries the account's credentials.
                JmapError::session(format!("unresolvable redirect to {location:?}"))
            })?;
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
        let served_by = Url::parse(&url)
            .map_err(|e| JmapError::session(format!("bad session URL {url:?}: {e}")))?;
        return Ok((transport::read_json(resp).await?, served_by));
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

#[cfg(test)]
#[path = "redirect_tests.rs"]
mod redirect_tests;
