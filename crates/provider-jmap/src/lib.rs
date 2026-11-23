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
//! - [`Request`], [`ResultRef`] and [`Response`] — the `{ using, methodCalls }` envelope, `#`
//!   result references, and the answer looked up by [`CallId`].
//! - [`Session`] — the session resource: capabilities, account ids ([`JmapAccountId`]), limits, the
//!   blob and push URL templates, and the [`SessionUrlPolicy`] for resolving advertised URLs.
//! - [`blob`] — the download and upload URLs, and a download read as a stream.
//! - [`JmapClient`] — connect, execute, upload and download: the low-level handle the normalization
//!   and `Provider` impl build on, public for a host that needs JMAP the [`JmapProvider`] does not
//!   wrap (a server's extension, Sieve, a vacation response).
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
mod base64;
pub mod blob;
mod blob_batch;
mod calendar;
mod calendar_conference;
mod calendar_conference_write;
mod calendar_patch;
mod calendar_rights;
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
mod email_band;
mod email_window;
mod error;
mod executor;
mod fetch;
mod fetch_scope;
mod identity;
#[cfg(feature = "import")]
mod import;
mod json;
mod mail;
mod mailbox_write;
mod mutate;
mod participant_identity;
mod provider;
mod provider_calendar;
mod report;
mod request;
mod response;
mod session;
mod session_accounts;
mod session_limits;
mod session_urls;
mod shared;
mod submit;
mod submit_body;
mod sync_ops;
mod transport;
mod walk;
mod watch;

use core::fmt;

pub use config::{Credentials, JmapConfig};
use engine_core::ids::BlobId;
use engine_http::Exchange;
use engine_provider::{ConnectObserver, ConnectStep, IgnoreConnectSteps};
pub use error::{JmapError, ServerText};
pub use provider::JmapProvider;
pub use request::{CallId, Request, ResultRef, capability};
use reqwest::Url;
pub use response::Response;
pub use session::{CoreLimits, Session, SessionUrlPolicy};
pub use session_accounts::JmapAccountId;
pub use watch::{DEFAULT_EVENT_SOURCE_PING, JmapWatcher};

use crate::{blob::BlobDownload, session_urls::resolve_against, transport::Transport};

/// The maximum number of redirects followed while discovering the session
/// resource (the well-known endpoint 307-redirects to the session URL).
const MAX_SESSION_REDIRECTS: usize = 5;

/// A connected JMAP client: an authenticated transport plus the resolved session.
///
/// Built with [`JmapClient::connect`], which fetches and resolves the session. It runs a
/// [`Request`] ([`execute`](Self::execute)) and moves blobs ([`upload`](Self::upload),
/// [`download`](Self::download), [`download_stream`](Self::download_stream)), every request
/// authenticated, inside the account's concurrency gate, and throttled as the shared HTTP funnel
/// throttles (`docs/agent-guidance/http-throttling.md`). [`JmapProvider`] is built on it; a host
/// reaches for it directly for the JMAP the provider does not wrap.
///
/// # Examples
///
/// ```no_run
/// use provider_jmap::{Credentials, JmapClient, JmapConfig, Request, capability};
/// use serde_json::json;
///
/// # async fn example(user_agent: engine_http::UserAgent) -> Result<(), provider_jmap::JmapError> {
/// let config = JmapConfig::new(
///     "https://jmap.example.com",
///     Credentials::bearer("token"),
///     user_agent,
/// );
/// let client = JmapClient::connect(config).await?;
/// let account = client.session().mail_account_id()?;
/// let mut request = Request::new([capability::CORE, capability::MAIL]);
/// let query = request.invoke("Email/query", json!({ "accountId": account, "limit": 20 }));
/// let response = client.execute(&request).await?;
/// let ids = &response.result(&query)?["ids"];
/// # Ok(()) }
/// ```
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
        let transport = Transport::new(
            config.credentials,
            &config.tls,
            &config.retry,
            &config.user_agent,
        )?;
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
            config.shared_account.as_ref(),
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

    /// Sends `request` to the API endpoint and parses the answer. A method the server answered
    /// with an error reads as one where its result is read ([`Response::result`]).
    ///
    /// # Errors
    ///
    /// [`JmapError`] on a transport or HTTP failure, or an answer that is not a JMAP response.
    pub async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        self.send(request, Exchange::Ordinary).await
    }

    /// [`execute`](Self::execute), where `exchange` says whether the request submits a message
    /// (`crate::submit`).
    pub(crate) async fn send(
        &self,
        request: &Request,
        exchange: Exchange<'_>,
    ) -> Result<Response, JmapError> {
        let body = request.to_json();
        let value = (self.transport)
            .post_json(self.session.api_url(), &body, exchange)
            .await?;
        Response::parse(&value)
    }

    /// Downloads the blob at `url`, whole: a URL [`blob::download_url`] built from this
    /// session's template (RFC 8620 §6.2).
    ///
    /// For a blob too large to hold, [`download_stream`](Self::download_stream) reads it a piece
    /// at a time.
    ///
    /// # Errors
    ///
    /// [`JmapError::Refused`] when `url` is not on the origin of the session's `downloadUrl`
    /// (the account's credential goes nowhere else), [`JmapError::Session`] when the session
    /// advertised none, and [`JmapError`] on a transport failure or a non-success status.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use engine_core::ids::BlobId;
    /// use provider_jmap::{JmapClient, blob};
    ///
    /// # async fn example(client: &JmapClient, id: &BlobId) -> Result<(), Box<dyn std::error::Error>> {
    /// let session = client.session();
    /// let template = session.download_url().ok_or("no downloadUrl")?;
    /// let account = session.mail_account_id()?;
    /// let url = blob::download_url(template, account, id, "message/rfc822", "message.eml");
    /// let bytes = client.download(&url).await?;
    /// # Ok(()) }
    /// ```
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        on_advertised_origin(url, self.session.download_url(), "downloadUrl")?;
        self.transport.get_bytes(url).await
    }

    /// Starts downloading the blob at `url`, to be read a piece at a time, so a large blob is
    /// never whole in memory. The URL is checked as [`download`](Self::download) checks it, and
    /// the status before anything is returned.
    ///
    /// # Errors
    ///
    /// As [`download`](Self::download).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use provider_jmap::{JmapClient, JmapError};
    ///
    /// async fn copy(client: &JmapClient, url: &str, out: &mut Vec<u8>) -> Result<(), JmapError> {
    ///     let mut blob = client.download_stream(url).await?;
    ///     while let Some(chunk) = blob.next_chunk().await? {
    ///         out.extend_from_slice(&chunk);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub async fn download_stream(&self, url: &str) -> Result<BlobDownload, JmapError> {
        on_advertised_origin(url, self.session.download_url(), "downloadUrl")?;
        Ok(BlobDownload::new(self.transport.get_streaming(url).await?))
    }

    /// Uploads `bytes` of `media_type` to `url` (a URL [`blob::upload_url`] built from this
    /// session's template), and returns the id the server stored them under (RFC 8620 §6.1).
    /// A later call references the blob by it: an `Email/import`, or an attachment in an
    /// `Email/set`.
    ///
    /// # Errors
    ///
    /// [`JmapError::Refused`] when `url` is not on the origin of the session's `uploadUrl`,
    /// [`JmapError::Session`] when the session advertised none, [`JmapError::Protocol`] when
    /// the answer names no usable `blobId`, and [`JmapError`] on a transport failure or a
    /// non-success status: a `429` here can be the server's upload quota, which clears only
    /// when its oldest upload expires, and carries the server's `Retry-After`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use provider_jmap::{JmapClient, blob};
    ///
    /// # async fn example(client: &JmapClient, raw: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    /// let session = client.session();
    /// let template = session.upload_url().ok_or("no uploadUrl")?;
    /// let url = blob::upload_url(template, session.mail_account_id()?);
    /// let blob = client.upload(&url, "message/rfc822", raw).await?;
    /// # Ok(()) }
    /// ```
    pub async fn upload(
        &self,
        url: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<BlobId, JmapError> {
        on_advertised_origin(url, self.session.upload_url(), "uploadUrl")?;
        let value = self
            .transport
            .post_bytes(url, media_type, bytes.to_vec())
            .await?;
        value
            .get("blobId")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| BlobId::try_from(id).ok())
            .ok_or_else(|| JmapError::protocol("upload response missing blobId"))
    }
}

/// Refuses a blob request to anywhere but the origin of the session's `template`: the URL
/// carries the account's credential, and the session is the only party that names where it may
/// go.
fn on_advertised_origin(url: &str, template: Option<&str>, which: &str) -> Result<(), JmapError> {
    let template =
        template.ok_or_else(|| JmapError::session(format!("server advertised no {which}")))?;
    if engine_provider::same_origin(url, template) {
        Ok(())
    } else {
        Err(JmapError::Refused(format!(
            "a blob request goes only to the origin of the session's {which}"
        )))
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
            // Unparseable, or a hop off TLS that `redirect_target` cannot take over TLS: every
            // one of these requests carries the account's credentials.
            let next = engine_provider::redirect_target(&url, location)
                .ok_or_else(|| JmapError::session(engine_provider::redirect_refused(location)))?;
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
    let _ = response::Response::parse(&value);
    let _ = sync_ops::Changes::parse(&value);
}

#[cfg(test)]
mod process_kill_tests;

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "redirect_tests.rs"]
mod redirect_tests;

#[cfg(test)]
mod write_outcome_tests;
