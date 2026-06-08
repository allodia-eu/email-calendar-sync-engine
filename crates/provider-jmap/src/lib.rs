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
//! - `request` — the `{ using, methodCalls }` envelope, `#id` back-references,
//!   and typed response lookup.
//! - `session` — the session resource: capabilities, account ids, limits, and
//!   the [`SessionUrlPolicy`] for resolving advertised URLs.
//! - [`JmapClient`] — connect + execute, the low-level handle the normalization
//!   and `Provider` impl build on.
//!
//! # Two real-world notes
//!
//! - **Advertised origin ≠ connection origin.** Stalwart advertises
//!   `https://mail.test.local/` in its session while tests connect to
//!   `127.0.0.1:18080`; [`SessionUrlPolicy::RebaseToConnection`] (the default)
//!   keeps the path but forces the connection origin. Providers that genuinely
//!   serve their API cross-origin use [`SessionUrlPolicy::TrustAdvertised`].
//! - **Raw MIME is referenced, not yet stored.** A normalized mail object keeps
//!   its JMAP `blobId` so the raw RFC 5322 source can be fetched on demand; durable
//!   raw-MIME blob storage awaits the store's blob sub-step. Calendar raw
//!   (`RawJsCalendar`) *is* preserved on the object (`docs/agent-guidance/jmap.md`).

mod calendar;
mod error;
mod fetch;
mod json;
mod mail;
mod provider;
mod request;
mod session;
mod submit;
mod sync_ops;
mod transport;

pub use error::JmapError;
pub use provider::JmapProvider;
pub use session::{CoreLimits, Session, SessionUrlPolicy};

use core::fmt;

use reqwest::Url;

use crate::request::{Request, Response};
use crate::session::resolve_against;
use crate::transport::Transport;

/// The maximum number of redirects followed while discovering the session
/// resource (the well-known endpoint 307-redirects to the session URL).
const MAX_SESSION_REDIRECTS: usize = 5;

/// Credentials for authenticating to a JMAP server.
///
/// `Debug` is redacted — the secret never appears in logs (`north-star.md`
/// security). Basic auth covers the Stalwart fixture; bearer covers OAuth
/// providers.
#[derive(Clone)]
pub enum Credentials {
    /// HTTP Basic credentials.
    Basic {
        /// The username (full email address for the fixture).
        username: String,
        /// The password or app-specific token.
        password: String,
    },
    /// An OAuth bearer token.
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
    /// # Errors
    ///
    /// Returns [`JmapError`] on a bad base URL, a transport/HTTP failure, or a
    /// malformed/incomplete session resource.
    pub async fn connect(config: JmapConfig) -> Result<Self, JmapError> {
        let base = Url::parse(&config.base_url)
            .map_err(|e| JmapError::session(format!("bad base_url {:?}: {e}", config.base_url)))?;
        let transport = Transport::new(config.credentials)?;
        let document =
            fetch_session(&transport, &base, &config.session_path, config.session_urls).await?;
        let session = Session::parse(&document, &base, config.session_urls)?;
        Ok(Self { transport, session })
    }

    /// The resolved session (capabilities, account ids, limits, API URL).
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
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
}

impl fmt::Debug for JmapClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JmapClient")
            .field("api_url", &self.session.api_url())
            .finish_non_exhaustive()
    }
}

/// Fetches the session document, resolving the well-known redirect chain itself so
/// a foreign advertised origin can be rebased onto the connection.
async fn fetch_session(
    transport: &Transport,
    base: &Url,
    session_path: &str,
    policy: SessionUrlPolicy,
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
            url = resolve_against(base, location, policy)?;
            continue;
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
mod tests {
    use super::*;

    #[test]
    fn credentials_debug_is_redacted() {
        let basic = Credentials::basic("alice@test.local", "harness-alice-pw");
        let shown = format!("{basic:?}");
        assert!(shown.contains("alice@test.local"));
        assert!(
            !shown.contains("harness-alice-pw"),
            "password must not leak: {shown}"
        );
        let bearer = Credentials::bearer("super-secret-token");
        let shown = format!("{bearer:?}");
        assert!(
            !shown.contains("super-secret-token"),
            "token must not leak: {shown}"
        );
    }

    #[test]
    fn config_debug_omits_credentials() {
        let config = JmapConfig::new(
            "http://127.0.0.1:18080",
            Credentials::basic("alice@test.local", "harness-alice-pw"),
        );
        let shown = format!("{config:?}");
        assert!(shown.contains("127.0.0.1:18080"));
        assert!(!shown.contains("harness-alice-pw"));
    }

    #[test]
    fn config_builder_overrides_defaults() {
        let config = JmapConfig::new("http://h", Credentials::bearer("t"))
            .with_session_path("/jmap/session")
            .with_session_urls(SessionUrlPolicy::TrustAdvertised);
        assert_eq!(config.session_path, "/jmap/session");
        assert_eq!(config.session_urls, SessionUrlPolicy::TrustAdvertised);
    }

    /// Hostile-input guard (the `fuzz/` cargo-fuzz target's in-gate counterpart):
    /// the JMAP parsers must return errors, never panic, on adversarial JSON.
    #[test]
    fn parsers_never_panic_on_hostile_json() {
        use serde_json::json;
        let adversarial = [
            json!(null),
            json!(7),
            json!("x"),
            json!([]),
            json!({}),
            json!({ "id": 123 }),
            json!({ "mailboxIds": "nope", "keywords": 5 }),
            json!({ "id": "e", "calendarIds": { "c": true }, "start": "not-a-date" }),
            json!({ "id": "e", "uid": "u", "calendarIds": { "c": true }, "start": "2026-13-40T99:99:99" }),
            json!({ "recurrenceRule": { "frequency": "fortnightly" } }),
            json!({ "id": "e", "uid": "u", "calendarIds": { "c": true }, "start": "2026-01-01T00:00:00",
                    "recurrenceOverrides": { "bad-rid": { "start": "also-bad" } } }),
            json!({ "methodResponses": "not-an-array" }),
            json!({ "methodResponses": [["only-two", {}]] }),
            json!({ "created": [1, 2, 3], "newState": 9 }),
            json!({ "participants": { "p": { "calendarAddress": 5, "roles": "nope" } } }),
        ];
        for case in &adversarial {
            let _ = mail::mailbox_from_json(case);
            let _ = mail::message_from_json(case);
            let _ = calendar::calendar_from_json(case);
            let _ = calendar::event_from_json(case);
            let _ = request::Response::parse(case);
            let _ = sync_ops::Changes::parse(case);
        }
    }

    #[test]
    fn raw_bytes_never_panic_through_the_pipeline() {
        for raw in [
            b"".as_slice(),
            b"{",
            b"[1,2,",
            b"\xff\xfe\x00",
            b"1e9999",
            br#"{"start":"2026-02-30T00:00:00","timeZone":""}"#,
        ] {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) {
                let _ = calendar::event_from_json(&value);
                let _ = mail::message_from_json(&value);
            }
        }
    }

    #[cfg(feature = "fuzzing")]
    #[test]
    fn fuzz_entry_point_runs_without_panicking() {
        // Drive the fuzz entry the cargo-fuzz target calls, so it is covered under
        // `--all-features` even without nightly.
        for raw in [
            br#"{"id":"e","mailboxIds":{"a":true}}"#.as_slice(),
            b"garbage",
            b"{}",
        ] {
            fuzz_parse(raw);
        }
    }

    // A blocking single-shot mock HTTP server lets the live-only transport,
    // session discovery, and `execute` be exercised offline (no harness).
    fn mock_server(http_responses: Vec<String>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in http_responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    const SESSION_DOC: &str = r#"{"capabilities":{"urn:ietf:params:jmap:core":{"maxObjectsInGet":500},"urn:ietf:params:jmap:mail":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c"},"apiUrl":"https://mail.test.local/jmap/"}"#;

    #[tokio::test]
    async fn connect_and_execute_against_a_mock_server() {
        let api = r#"{"methodResponses":[["Mailbox/get",{"state":"s1","list":[]},"0"]]}"#;
        let base = mock_server(vec![http_ok(SESSION_DOC), http_ok(api)]);
        let client = JmapClient::connect(
            JmapConfig::new(base, Credentials::basic("alice", "pw"))
                .with_session_path("/jmap/session"),
        )
        .await
        .unwrap();
        assert!(client.session().capabilities().mail());
        assert!(format!("{client:?}").contains("JmapClient"));

        let mut req = request::Request::new([request::capability::CORE]);
        req.invoke("Mailbox/get", serde_json::json!({ "accountId": "c" }));
        let resp = client.execute(&req).await.unwrap();
        assert!(resp.result("0").is_ok());
    }

    #[tokio::test]
    async fn connect_follows_the_well_known_redirect() {
        let redirect = "HTTP/1.1 307 Temporary Redirect\r\nLocation: /jmap/session\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let base = mock_server(vec![redirect.to_owned(), http_ok(SESSION_DOC)]);
        // Default session path is /.well-known/jmap → 307 → /jmap/session (rebased).
        let client = JmapClient::connect(JmapConfig::new(base, Credentials::basic("a", "b")))
            .await
            .unwrap();
        assert!(client.session().capabilities().mail());
    }

    #[tokio::test]
    async fn http_error_status_surfaces_as_a_classified_error() {
        let body = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\nConnection: close\r\n\r\nerr";
        let base = mock_server(vec![body.to_owned()]);
        let err = JmapClient::connect(
            JmapConfig::new(base, Credentials::basic("a", "b")).with_session_path("/jmap/session"),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::Retryable
        );
    }

    #[tokio::test]
    async fn jmap_provider_connects_and_syncs_through_the_real_client() {
        use engine_provider::Provider;
        let mailboxes = r#"{"methodResponses":[["Mailbox/get",{"state":"s1","list":[]},"0"]]}"#;
        let base = mock_server(vec![http_ok(SESSION_DOC), http_ok(mailboxes)]);
        let provider = JmapProvider::connect(
            JmapConfig::new(base, Credentials::basic("a", "b")).with_session_path("/jmap/session"),
        )
        .await
        .unwrap();
        assert!(format!("{provider:?}").contains("JmapProvider"));
        let account = engine_core::ids::AccountId::try_from("acct").unwrap();
        assert!(
            provider
                .sync_mailboxes(&account, None)
                .await
                .unwrap()
                .is_snapshot()
        );
    }

    #[tokio::test]
    async fn transport_connect_failure_is_retryable() {
        // A refused connection surfaces as a retryable transport error.
        let err = JmapClient::connect(
            JmapConfig::new("http://127.0.0.1:1", Credentials::basic("a", "b"))
                .with_session_path("/jmap/session"),
        )
        .await
        .unwrap_err();
        assert!(err.failure_class().is_retryable());
    }

    #[tokio::test]
    async fn malformed_session_body_is_a_permanent_decode_error() {
        let base = mock_server(vec![http_ok("this is not json")]);
        let err = JmapClient::connect(
            JmapConfig::new(base, Credentials::basic("a", "b")).with_session_path("/jmap/session"),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::Permanent
        );
    }

    #[tokio::test]
    async fn bearer_auth_connects() {
        let base = mock_server(vec![http_ok(SESSION_DOC)]);
        let client = JmapClient::connect(
            JmapConfig::new(base, Credentials::bearer("tok")).with_session_path("/jmap/session"),
        )
        .await
        .unwrap();
        assert!(client.session().capabilities().mail());
    }
}
