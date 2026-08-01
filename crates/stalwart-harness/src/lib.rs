//! Test-support harness for the deterministic Stalwart protocol fixture.
//!
//! This crate is the Rust side of the Stalwart Docker harness described in
//! `docs/agent-guidance/stalwart-harness.md`. It does three things and nothing
//! more:
//!
//! 1. **Discovery + gating.** [`Harness::from_env`] returns `Some` only when `STALWART_HTTP_ADDR`
//!    is set. Every Stalwart-touching test starts by calling it and *skips* (returns early) when it
//!    is `None`, so the offline `cargo test --workspace` stays green with no Docker. The harness
//!    server is opt-in; absence is the skip signal.
//! 2. **Readiness.** [`Harness::wait_until_ready`] polls the plaintext `/healthz/live` endpoint — a
//!    real signal, not a sleep.
//! 3. **Probes.** Minimal JMAP / IMAP / SMTP / CalDAV / CardDAV checks used by the connectivity
//!    smoke suite (`tests/smoke.rs`). They assert on content the harness controls (subjects,
//!    `Message-ID`s, iCalendar UIDs, counts), never on server-assigned ids (JMAP id, IMAP UID, DAV
//!    ETag).
//!
//! The deep protocol suites (JMAP `Email/changes`, IMAP `UIDVALIDITY`, …) are
//! the provider clients' job in build-order steps 4–5; this crate only proves
//! the fixture is up, reachable on every protocol, and seeded.

mod http;
mod imap;
mod smtp;

use std::time::{Duration, Instant};

pub use http::{HttpResponse, base64_encode};
pub use imap::{ImapProbe, run_probe};

/// Environment variable whose presence gates all Stalwart-dependent tests.
pub const GATE_VAR: &str = "STALWART_HTTP_ADDR";

// Defaults match the host port mapping in docker/stalwart/docker-compose.yml.
// IMAP is implicit-TLS (Stalwart's v0.16 default), so its address is the TLS
// port; SMTP is plaintext.
const DEFAULT_IMAP_ADDR: &str = "127.0.0.1:11993";
const DEFAULT_SMTP_ADDR: &str = "127.0.0.1:11025";
// Implicit-TLS submission (465) is a Stalwart default listener; STARTTLS listeners
// (143/587) are provisioned by the entrypoint (Stalwart recommends implicit TLS, so a
// fresh bootstrap does not create them). Together they cover every IMAP/SMTP transport.
const DEFAULT_SMTP_TLS_ADDR: &str = "127.0.0.1:11465";
const DEFAULT_IMAP_STARTTLS_ADDR: &str = "127.0.0.1:11143";
const DEFAULT_SMTP_STARTTLS_ADDR: &str = "127.0.0.1:11587";
const DEFAULT_ACCOUNT: &str = "alice@test.local";
const DEFAULT_PASSWORD: &str = "harness-alice-pw";

/// iCalendar UID of the seeded one-off event (`one-off.ics`), used to fetch a
/// known calendar resource back over CalDAV.
pub const ONE_OFF_EVENT_UID: &str = "oneoff-2001";
/// Stable file name of the seeded person card.
pub const CONTACT_UID: &str = "contact-3001";

/// Errors raised by the harness probes.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// A socket connect/read/write failed.
    #[error("i/o error talking to {addr}: {source}")]
    Io {
        /// The address being contacted when the error occurred.
        addr: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A response was received but did not match the protocol.
    #[error("unexpected {protocol} response: {detail}")]
    Protocol {
        /// Which protocol produced the unexpected response.
        protocol: &'static str,
        /// What was wrong.
        detail: String,
    },
    /// Readiness was not reached within the allotted time.
    #[error("timed out after {0:?} waiting for /healthz/live")]
    Timeout(Duration),
    /// The JMAP session resource was not valid JSON.
    #[error("malformed JMAP session JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl HarnessError {
    /// Construct an [`HarnessError::Io`] for `addr`.
    pub(crate) fn io(addr: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            addr: addr.into(),
            source,
        }
    }

    /// Construct an [`HarnessError::Protocol`] for `protocol`.
    pub(crate) fn protocol(protocol: &'static str, detail: impl Into<String>) -> Self {
        Self::Protocol {
            protocol,
            detail: detail.into(),
        }
    }
}

/// Connection coordinates for a running Stalwart harness.
#[derive(Debug, Clone)]
pub struct Harness {
    /// `host:port` of the plaintext HTTP listener (JMAP + CalDAV + management).
    pub http_addr: String,
    /// `host:port` of the IMAP listener (implicit TLS).
    pub imap_addr: String,
    /// `host:port` of the plaintext SMTP listener.
    pub smtp_addr: String,
    /// `host:port` of the SMTP submission listener over implicit TLS (port 465).
    pub smtp_tls_addr: String,
    /// `host:port` of the IMAP STARTTLS listener (cleartext connect, upgraded).
    pub imap_starttls_addr: String,
    /// `host:port` of the SMTP submission STARTTLS listener.
    pub smtp_starttls_addr: String,
    /// Seeded account used to authenticate (full email address).
    pub account: String,
    /// The account's password.
    pub password: String,
    /// The accounts holding **none** of the seeded dataset (`bob@`, `carol@`), for a
    /// scenario that needs to mutate state no assertion counts.
    ///
    /// The shared dataset all lives in [`account`](Self::account), and several suites
    /// assert its exact mailbox and calendar counts. A scheduling run cannot live there:
    /// RFC 6638 auto-scheduling has the server **mail** the attendee an invitation *and*
    /// the organizer a reply, and those land in the INBOX — so a run between Alice and
    /// anyone would grow the count-asserted folder on every pass. Two scratch accounts let
    /// the whole two-party exchange happen off to the side.
    pub scratch: [ScratchAccount; 2],
}

/// An account carrying none of the seeded dataset — free to be written to.
#[derive(Debug, Clone)]
pub struct ScratchAccount {
    /// The full email address, which is also the DAV principal name.
    pub address: String,
    /// The account's password.
    pub password: String,
}

impl ScratchAccount {
    fn new(address: &str, password: &str) -> Self {
        Self {
            address: address.to_owned(),
            password: password.to_owned(),
        }
    }

    /// Basic-auth credentials for this account.
    #[must_use]
    pub fn auth(&self) -> (&str, &str) {
        (self.address.as_str(), self.password.as_str())
    }
}

impl Harness {
    /// Read harness coordinates from the environment, or `None` when
    /// [`GATE_VAR`] is unset — the signal that no server is available and
    /// Stalwart-dependent tests must skip.
    ///
    /// Only [`GATE_VAR`] is required; the other coordinates default to the
    /// values the bundled `docker compose` produces.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let http_addr = std::env::var(GATE_VAR).ok().filter(|v| !v.is_empty())?;
        let var = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        Some(Self {
            http_addr,
            imap_addr: var("STALWART_IMAP_ADDR", DEFAULT_IMAP_ADDR),
            smtp_addr: var("STALWART_SMTP_ADDR", DEFAULT_SMTP_ADDR),
            smtp_tls_addr: var("STALWART_SMTP_TLS_ADDR", DEFAULT_SMTP_TLS_ADDR),
            imap_starttls_addr: var("STALWART_IMAP_STARTTLS_ADDR", DEFAULT_IMAP_STARTTLS_ADDR),
            smtp_starttls_addr: var("STALWART_SMTP_STARTTLS_ADDR", DEFAULT_SMTP_STARTTLS_ADDR),
            account: var("STALWART_ACCOUNT", DEFAULT_ACCOUNT),
            password: var("STALWART_PASSWORD", DEFAULT_PASSWORD),
            // Not env-overridable, unlike the seeded account: these exist only because the
            // bundled entrypoint provisions them, so naming others would just 401.
            scratch: [
                ScratchAccount::new("bob@test.local", "harness-bob-pw"),
                ScratchAccount::new("carol@test.local", "harness-carol-pw"),
            ],
        })
    }

    /// Path of the seeded account's default calendar collection.
    #[must_use]
    pub fn calendar_collection_path(&self) -> String {
        Self::calendar_collection_path_of(&self.account)
    }

    /// Path of `account`'s default calendar collection.
    ///
    /// Takes the account rather than reading `self`, so a two-party scenario can address a
    /// [`scratch`](Self::scratch) account's collection as well.
    #[must_use]
    pub fn calendar_collection_path_of(account: &str) -> String {
        format!("/dav/cal/{account}/default/")
    }

    /// Path of `account`'s RFC 6638 **scheduling inbox** — where an auto-schedule server
    /// deposits the iTIP messages addressed to it.
    ///
    /// Stalwart advertises `calendar-auto-schedule` and reports this collection as
    /// `CALDAV:schedule-inbox-URL` on the calendar home; the path is hard-coded here for
    /// the same reason the calendar path is — a test asserts on harness-controlled
    /// structure, and discovering it would test discovery rather than scheduling.
    #[must_use]
    pub fn scheduling_inbox_path_of(account: &str) -> String {
        format!("/dav/itip/{account}/inbox/")
    }

    /// CalDAV path of a seeded event resource by its iCalendar UID.
    #[must_use]
    pub fn event_path(&self, uid: &str) -> String {
        format!("{}{uid}.ics", self.calendar_collection_path())
    }

    /// Path of the seeded account's default address-book collection.
    #[must_use]
    pub fn address_book_collection_path(&self) -> String {
        format!("/dav/card/{}/default/", self.account)
    }

    /// CardDAV path of a seeded vCard resource.
    #[must_use]
    pub fn contact_path(&self, uid: &str) -> String {
        format!("{}{uid}.vcf", self.address_book_collection_path())
    }

    /// Basic-auth credentials for the seeded account.
    fn auth(&self) -> (&str, &str) {
        (self.account.as_str(), self.password.as_str())
    }

    /// `GET /healthz/live` once.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn healthz(&self) -> Result<HttpResponse, HarnessError> {
        http::request(&self.http_addr, "GET", "/healthz/live", None, &[], &[])
    }

    /// Poll `/healthz/live` until it returns `200` or `timeout` elapses.
    ///
    /// # Errors
    /// Returns [`HarnessError::Timeout`] if readiness is not reached in time.
    pub fn wait_until_ready(&self, timeout: Duration) -> Result<(), HarnessError> {
        let deadline = Instant::now() + timeout;
        loop {
            if matches!(self.healthz(), Ok(resp) if resp.status == 200) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::Timeout(timeout));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Fetch and parse the JMAP session resource.
    ///
    /// `/.well-known/jmap` 307-redirects to `/jmap/session`; this requests the
    /// session resource directly (the probe does not follow redirects).
    ///
    /// # Errors
    /// Returns [`HarnessError::Protocol`] on a non-200 status and
    /// [`HarnessError::Json`] if the body is not valid JSON.
    pub fn jmap_session(&self) -> Result<serde_json::Value, HarnessError> {
        let resp = http::request(
            &self.http_addr,
            "GET",
            "/jmap/session",
            Some(self.auth()),
            &[],
            &[],
        )?;
        if resp.status != 200 {
            return Err(HarnessError::protocol(
                "jmap",
                format!("session returned HTTP {}", resp.status),
            ));
        }
        Ok(serde_json::from_slice(&resp.body)?)
    }

    /// Probe SMTP: read the banner and confirm `EHLO` is answered.
    ///
    /// # Errors
    /// Propagates [`HarnessError`] on transport or protocol mismatch.
    pub fn smtp_banner(&self) -> Result<String, HarnessError> {
        smtp::banner(&self.smtp_addr)
    }

    /// `PROPFIND` a CalDAV path (Depth 1) as the seeded account.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn caldav_propfind(&self, path: &str) -> Result<HttpResponse, HarnessError> {
        self.dav_propfind_as(self.auth(), path)
    }

    /// `PROPFIND` a CalDAV path (Depth 1) as `auth` — any account, not only the seeded one.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn dav_propfind_as(
        &self,
        auth: (&str, &str),
        path: &str,
    ) -> Result<HttpResponse, HarnessError> {
        let body = br#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getetag/></d:prop></d:propfind>"#;
        http::request(
            &self.http_addr,
            "PROPFIND",
            path,
            Some(auth),
            &[("Depth", "1"), ("Content-Type", "application/xml")],
            body,
        )
    }

    /// `GET` a CalDAV resource as the seeded account.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn caldav_get(&self, path: &str) -> Result<HttpResponse, HarnessError> {
        self.dav_get_as(self.auth(), path)
    }

    /// `GET` a DAV resource as `auth`.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn dav_get_as(&self, auth: (&str, &str), path: &str) -> Result<HttpResponse, HarnessError> {
        http::request(&self.http_addr, "GET", path, Some(auth), &[], &[])
    }

    /// `DELETE` a DAV resource as `auth`.
    ///
    /// Exists for scheduling-inbox housekeeping: RFC 6638 §3.2 makes *the client* responsible
    /// for removing an iTIP message once it has processed it, so a scheduling test that left
    /// its deliveries behind would grow the collection on every run.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn dav_delete_as(
        &self,
        auth: (&str, &str),
        path: &str,
    ) -> Result<HttpResponse, HarnessError> {
        http::request(&self.http_addr, "DELETE", path, Some(auth), &[], &[])
    }

    /// `POST` a raw JMAP request body to `/jmap/` as the seeded account.
    ///
    /// Exists to send what the adapter deliberately **cannot**. A live test that drives
    /// `JmapProvider` can only observe requests the adapter is willing to build, so it can
    /// never show what the server does with a precondition the adapter never sends — which is
    /// why the reason for omitting `ifInState` sat in prose until
    /// `provider-jmap/tests/live_calendar_precondition.rs` could demonstrate it.
    ///
    /// Reach for this only to probe wire-level server behaviour that no adapter surface
    /// expresses; anything the adapter *can* send belongs in a test that drives the adapter.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn jmap_post(&self, body: &[u8]) -> Result<HttpResponse, HarnessError> {
        http::request(
            &self.http_addr,
            "POST",
            "/jmap/",
            Some(self.auth()),
            &[("Content-Type", "application/json")],
            body,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Harness;

    // from_env reads GATE_VAR; with it unset, the harness is absent (skip).
    // Asserting the None path here keeps the gate itself covered offline. We do
    // not mutate process env (it would race other tests); we only assert the
    // gate var drives presence.
    #[test]
    fn from_env_absent_without_gate() {
        if std::env::var(super::GATE_VAR).is_err() {
            assert!(Harness::from_env().is_none());
        }
    }

    #[test]
    fn paths_are_built_from_account() {
        let h = Harness {
            http_addr: "127.0.0.1:18080".to_owned(),
            imap_addr: "127.0.0.1:11993".to_owned(),
            smtp_addr: "127.0.0.1:11025".to_owned(),
            smtp_tls_addr: "127.0.0.1:11465".to_owned(),
            imap_starttls_addr: "127.0.0.1:11143".to_owned(),
            smtp_starttls_addr: "127.0.0.1:11587".to_owned(),
            account: "alice@test.local".to_owned(),
            password: "pw".to_owned(),
            scratch: [
                super::ScratchAccount::new("bob@test.local", "bob-pw"),
                super::ScratchAccount::new("carol@test.local", "carol-pw"),
            ],
        };
        assert_eq!(
            h.calendar_collection_path(),
            "/dav/cal/alice@test.local/default/"
        );
        assert_eq!(
            h.event_path("oneoff-2001"),
            "/dav/cal/alice@test.local/default/oneoff-2001.ics"
        );
        // A scratch account's collections are addressable too — both sides of a
        // scheduling run happen off the seeded account entirely.
        assert_eq!(
            Harness::calendar_collection_path_of(&h.scratch[0].address),
            "/dav/cal/bob@test.local/default/"
        );
        assert_eq!(
            Harness::scheduling_inbox_path_of(&h.scratch[1].address),
            "/dav/itip/carol@test.local/inbox/"
        );
        assert_eq!(h.scratch[1].auth(), ("carol@test.local", "carol-pw"));
    }
}
