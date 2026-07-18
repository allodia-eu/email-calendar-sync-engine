//! [`ImapConfig`] — how to dial an IMAP account, and (optionally) how to submit over
//! SMTP.
//!
//! Split out of `provider` so that file stays under the size limit. The fields are
//! `pub(crate)`: `provider` reads them to dial, and `watch` hands the same config to
//! the shared [`connect_session`](crate::provider::connect_session) so a watcher's
//! dedicated connection is built exactly like the sync one.

use std::sync::Arc;

use engine_provider::ConnectObserver;

/// How the IMAP session is secured: TLS from the first byte (port 993), or a
/// cleartext connection upgraded in place with `STARTTLS` (port 143). Both present
/// [`ImapConfig::server_name`] for the handshake; the difference is only *when* the
/// handshake runs (`docs/agent-guidance/imap-smtp.md`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ImapSecurity {
    /// Implicit TLS: the socket is TLS-wrapped before the greeting (port 993).
    ImplicitTls,
    /// STARTTLS: connect in the clear, negotiate the TLS upgrade, then log in
    /// (port 143). Credentials never cross the wire before the upgrade.
    StartTls,
}

/// How SMTP submission is secured. Plaintext is the fixture's local MX (port 25, no
/// auth); the two TLS modes carry the `server_name` presented to the handshake and
/// authenticate with `AUTH PLAIN` — only ever over the established TLS.
#[derive(Clone)]
pub(crate) enum SmtpSecurity {
    /// Plaintext, no auth — an MX that accepts local mail (Stalwart's port 25).
    Plaintext,
    /// Implicit TLS from the first byte (port 465), then `AUTH PLAIN`.
    ImplicitTls { server_name: String },
    /// Cleartext connect upgraded with `STARTTLS` (port 587), then `AUTH PLAIN`.
    StartTls { server_name: String },
}

/// SMTP submission settings captured at config time: the address and how the
/// connection is secured ([`SmtpSecurity`]).
#[derive(Clone)]
pub(crate) struct SmtpSettings {
    pub(crate) addr: String,
    pub(crate) security: SmtpSecurity,
}

/// How to connect an [`ImapProvider`](crate::ImapProvider): the address, the TLS
/// server name, and credentials. `Debug` redacts the password (`north-star.md`
/// security).
#[derive(Clone)]
pub struct ImapConfig {
    pub(crate) addr: String,
    pub(crate) server_name: String,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) security: ImapSecurity,
    pub(crate) smtp: Option<SmtpSettings>,
    pub(crate) since: Option<time::Date>,
    pub(crate) connect_observer: Option<Arc<dyn ConnectObserver>>,
}

impl ImapConfig {
    /// Configures an implicit-TLS IMAP connection to `addr` (`host:port`),
    /// presenting `server_name` for TLS (SNI/cert name; may differ from a loopback
    /// `addr`) and authenticating as `username`/`password`.
    #[must_use]
    pub fn new(
        addr: impl Into<String>,
        server_name: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            addr: addr.into(),
            server_name: server_name.into(),
            username: username.into(),
            password: password.into(),
            security: ImapSecurity::ImplicitTls,
            smtp: None,
            since: None,
            connect_observer: None,
        }
    }

    /// Secures the IMAP session with **STARTTLS** (port 143) instead of implicit TLS:
    /// the client connects in the clear, then upgrades the socket to TLS with the
    /// injected connector before logging in, so credentials never cross the wire
    /// unencrypted. The connect **fails** if the server does not advertise `STARTTLS`
    /// (no silent downgrade). [`server_name`](Self::new) is presented to the handshake
    /// exactly as for implicit TLS.
    #[must_use]
    pub fn with_starttls(mut self) -> Self {
        self.security = ImapSecurity::StartTls;
        self
    }

    /// Bounds mail sync to messages delivered on or after `since` (the sync-depth
    /// window). A snapshot then fetches only mail within the window — so a large
    /// mailbox syncs just recent messages — via `UID SEARCH SINCE` to find the window
    /// floor. With no cutoff (the default) the whole mailbox syncs. A delta is already
    /// bounded to new arrivals, so the window never narrows it.
    #[must_use]
    pub fn with_since(mut self, since: time::Date) -> Self {
        self.since = Some(since);
        self
    }

    /// Enables **plaintext** SMTP submission via `smtp_addr` (`host:port`), with no
    /// authentication — for an MX that accepts local mail (the Stalwart fixture's
    /// port 25). Without any SMTP config the provider advertises no submission
    /// capability and [`submit_email`](engine_provider::Provider::submit_email) is
    /// rejected.
    #[must_use]
    pub fn with_smtp(mut self, smtp_addr: impl Into<String>) -> Self {
        self.smtp = Some(SmtpSettings {
            addr: smtp_addr.into(),
            security: SmtpSecurity::Plaintext,
        });
        self
    }

    /// Enables **implicit-TLS** SMTP submission via `smtp_addr` (`host:port`,
    /// typically `:465`), authenticating with `AUTH PLAIN` using the account
    /// credentials. The injected TLS connector (from
    /// [`ImapProvider::connect`](crate::ImapProvider::connect)) secures the
    /// connection from the first byte, presenting `server_name`.
    #[must_use]
    pub fn with_smtp_tls(
        mut self,
        smtp_addr: impl Into<String>,
        server_name: impl Into<String>,
    ) -> Self {
        self.smtp = Some(SmtpSettings {
            addr: smtp_addr.into(),
            security: SmtpSecurity::ImplicitTls {
                server_name: server_name.into(),
            },
        });
        self
    }

    /// Enables **STARTTLS** SMTP submission via `smtp_addr` (`host:port`, typically
    /// `:587`): the client connects in the clear, `EHLO`s, upgrades the socket with
    /// `STARTTLS`, re-`EHLO`s over TLS, and only then authenticates with `AUTH PLAIN`.
    /// The submission **fails** if the server does not advertise `STARTTLS` (no
    /// cleartext auth). The injected TLS connector presents `server_name`.
    #[must_use]
    pub fn with_smtp_starttls(
        mut self,
        smtp_addr: impl Into<String>,
        server_name: impl Into<String>,
    ) -> Self {
        self.smtp = Some(SmtpSettings {
            addr: smtp_addr.into(),
            security: SmtpSecurity::StartTls {
                server_name: server_name.into(),
            },
        });
        self
    }

    /// Observes the connect phase: [`ConnectStep::TlsEstablished`] once the handshake
    /// agrees a version, then [`ConnectStep::Authenticated`] once `LOGIN` succeeds.
    ///
    /// IMAP dials a known address and issues no discovery, so there is no
    /// [`Redirected`] or [`Discovered`] step. It is the one adapter that *can* report a
    /// TLS version: it owns a `tokio-rustls` stream, where the `reqwest`-backed
    /// adapters see only the peer certificate (`docs/agent-guidance/tls.md`).
    ///
    /// The observer rides on the config, so an [`ImapWatcher`](crate::ImapWatcher)'s
    /// dedicated connection — and any redial after a dropped session — is observed too.
    /// `Arc` so one host observer can be shared across the account's providers.
    ///
    /// [`ConnectStep::TlsEstablished`]: engine_provider::ConnectStep::TlsEstablished
    /// [`ConnectStep::Authenticated`]: engine_provider::ConnectStep::Authenticated
    /// [`Redirected`]: engine_provider::ConnectStep::Redirected
    /// [`Discovered`]: engine_provider::ConnectStep::Discovered
    #[must_use]
    pub fn with_connect_observer(mut self, observer: Arc<dyn ConnectObserver>) -> Self {
        self.connect_observer = Some(observer);
        self
    }

    /// The configured observer, or the no-op default — what the dial reports through.
    pub(crate) fn connect_observer(&self) -> &dyn ConnectObserver {
        self.connect_observer
            .as_deref()
            .unwrap_or(&engine_provider::IgnoreConnectSteps)
    }
}

impl core::fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapConfig")
            .field("addr", &self.addr)
            .field("server_name", &self.server_name)
            .field("username", &self.username)
            .field("security", &self.security)
            .field("since", &self.since)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use engine_provider::{ConnectStep, IgnoreConnectSteps};

    use super::*;

    #[test]
    fn debug_redacts_the_password() {
        let config = ImapConfig::new("mail.test:993", "mail.test", "alice", "hunter2");
        let shown = format!("{config:?}");
        assert!(shown.contains("alice") && shown.contains("mail.test:993"));
        assert!(
            !shown.contains("hunter2"),
            "password must not leak: {shown}"
        );
    }

    #[test]
    fn the_builders_set_the_dial_settings_they_name() {
        let since = time::Date::from_calendar_date(2026, time::Month::March, 18).unwrap();
        let plain = ImapConfig::new("h:993", "h", "u", "p")
            .with_since(since)
            .with_smtp("h:25");
        assert_eq!(plain.since, Some(since));
        // IMAP defaults to implicit TLS until `with_starttls` flips it.
        assert_eq!(plain.security, ImapSecurity::ImplicitTls);
        let smtp = plain.smtp.expect("smtp configured");
        assert_eq!(smtp.addr, "h:25");
        // Plaintext MX path (no implicit TLS, no `AUTH PLAIN`).
        assert!(matches!(smtp.security, SmtpSecurity::Plaintext));

        let tls = ImapConfig::new("h:993", "h", "u", "p")
            .with_smtp_tls("h:465", "smtp.example.com")
            .smtp
            .expect("smtp configured");
        assert_eq!(tls.addr, "h:465");
        assert!(
            matches!(tls.security, SmtpSecurity::ImplicitTls { server_name } if server_name == "smtp.example.com")
        );

        let starttls = ImapConfig::new("h:143", "h", "u", "p")
            .with_starttls()
            .with_smtp_starttls("h:587", "smtp.example.com");
        assert_eq!(starttls.security, ImapSecurity::StartTls);
        let smtp = starttls.smtp.expect("smtp configured");
        assert_eq!(smtp.addr, "h:587");
        assert!(
            matches!(smtp.security, SmtpSecurity::StartTls { server_name } if server_name == "smtp.example.com")
        );
    }

    #[test]
    fn a_config_without_an_observer_reports_through_the_no_op_default() {
        let config = ImapConfig::new("h:993", "h", "u", "p");
        assert!(config.connect_observer.is_none());
        // Exercised rather than asserted on: the default's contract is that it does
        // nothing and does not panic.
        config.connect_observer().step(&ConnectStep::Authenticated);
        IgnoreConnectSteps.step(&ConnectStep::Authenticated);
    }

    #[test]
    fn the_configured_observer_is_the_one_the_dial_reports_through() {
        let seen = Arc::new(std::sync::Mutex::new(0_usize));
        let counter = Arc::clone(&seen);
        let config = ImapConfig::new("h:993", "h", "u", "p").with_connect_observer(Arc::new(
            move |_: &ConnectStep<'_>| *counter.lock().unwrap() += 1,
        ));
        config.connect_observer().step(&ConnectStep::Authenticated);
        assert_eq!(*seen.lock().unwrap(), 1);
    }
}
