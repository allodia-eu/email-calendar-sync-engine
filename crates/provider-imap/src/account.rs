//! [`ImapAccount`] — the one handle per account that every folder's provider and watcher share.
//!
//! An [`ImapProvider`] is bound to one mailbox, because IMAP's sync state is per folder, and a
//! host builds one per folder it syncs. What must *not* be per folder is the socket: an account
//! whose providers each dialled their own holds one connection per folder whether it is syncing
//! or idle, and re-dials all of them at once every time the network drops (see
//! [`crate::pool`] for why a server's limit cannot be discovered, only avoided).
//!
//! So connecting is split in two. [`ImapAccount::connect`] dials once — which is what proves the
//! credentials and reads the server's capabilities — and keeps that connection in the account's
//! pool. [`ImapAccount::provider`] and [`ImapAccount::watch`] then hand out folder-bound views
//! that borrow from the pool, so however many folders a host binds, the account's sockets stay
//! under one budget.

use std::{sync::Arc, time::Duration};

use engine_core::ids::MailboxId;
use engine_provider::{
    Capabilities, ConnectionInfo, ProviderResult, ReportControls, ReportEvidence, ReportVerdicts,
    TlsVersion,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::{
    config::ImapConfig,
    connect::connect_session,
    error::ImapError,
    filing::{SmtpSender, resolve_smtp},
    pool::{DEFAULT_MAX_CONNECTIONS, Dial, ImapPool, VALIDATE_AFTER_REST},
    provider::ImapProvider,
    transport::Connection,
    watch::ImapWatcher,
};

/// One IMAP account: its pool of authenticated connections, and what every folder's provider
/// needs to know about the server.
///
/// Build **one per account** and bind every folder through it. Two `ImapAccount`s for the same
/// account are two budgets, which is the bug this type exists to prevent.
pub struct ImapAccount<S> {
    pool: Arc<ImapPool<S>>,
    /// Shared by every provider this account binds; each send dials its own SMTP connection.
    smtp: Option<Arc<SmtpSender>>,
    /// The sync-depth floor from `ImapConfig::with_since`, handed to every provider.
    since: Option<time::Date>,
    /// The capabilities and TLS version the first connection negotiated. Every connection in
    /// the pool dials the same server as the same user, so this describes all of them.
    connection_info: ConnectionInfo,
}

impl<S> core::fmt::Debug for ImapAccount<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapAccount")
            .field("pool", &self.pool)
            .field("since", &self.since)
            .field("connection_info", &self.connection_info)
            .finish_non_exhaustive()
    }
}

impl ImapAccount<TlsStream<TcpStream>> {
    /// Connects over TLS (implicit or STARTTLS, per the config) and logs in, proving the
    /// credentials and reading the server's capabilities. The connection is kept for the first
    /// folder that needs one.
    ///
    /// The `connector` carries the host's trust policy — the library never bakes in a root
    /// store, so a mobile host (or the self-signed test fixture) injects its own
    /// (`docs/agent-guidance/imap-smtp.md`). Every later connection the account dials uses it
    /// too, and reports through the config's connect observer, so a host sees each socket the
    /// account opens.
    ///
    /// # Errors
    ///
    /// [`ImapError`] on a TCP/TLS/login failure or a bad server name.
    pub async fn connect(config: &ImapConfig, connector: TlsConnector) -> Result<Self, ImapError> {
        let smtp = config
            .smtp
            .as_ref()
            .map(|settings| resolve_smtp(settings, &connector, config));
        let (connection, tls_version) = connect_session(config, &connector).await?;
        let dial = dial_tls(config.clone(), connector);
        Ok(Self::build(
            connection,
            dial,
            smtp,
            config.since,
            tls_version,
        ))
    }
}

/// The pool's dial for a live account: the same TCP + TLS + `LOGIN` sequence as the first
/// connection, from the same config.
fn dial_tls(config: ImapConfig, connector: TlsConnector) -> Dial<TlsStream<TcpStream>> {
    let config = Arc::new(config);
    Arc::new(move || {
        let config = Arc::clone(&config);
        let connector = connector.clone();
        Box::pin(async move {
            connect_session(&config, &connector)
                .await
                .map(|(connection, _tls_version)| connection)
        })
    })
}

impl<S> ImapAccount<S> {
    /// Builds an account around its first `connection`, advertising submission iff SMTP is
    /// configured and recording the `tls_version` the dial negotiated (`None` when the stream
    /// is not TLS — the offline mock).
    fn build(
        connection: Connection<S>,
        dial: Dial<S>,
        smtp: Option<SmtpSender>,
        since: Option<time::Date>,
        tls_version: Option<TlsVersion>,
    ) -> Self {
        let capabilities = capabilities(&connection, smtp.is_some());
        let pool = ImapPool::new(dial, DEFAULT_MAX_CONNECTIONS, VALIDATE_AFTER_REST);
        pool.adopt(connection);
        Self {
            pool,
            smtp: smtp.map(Arc::new),
            since,
            connection_info: ConnectionInfo {
                tls_version,
                ..ConnectionInfo::new(capabilities)
            },
        }
    }

    /// An account over an already-open, logged-in mock connection, whose pool cannot dial a
    /// second: once the one connection is spent, a call fails the way it would with no network.
    #[cfg(test)]
    pub(crate) fn offline(connection: Connection<S>, smtp: Option<SmtpSender>) -> Self
    where
        S: 'static,
    {
        let dial: Dial<S> = Arc::new(|| {
            Box::pin(async {
                Err(ImapError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "an offline test account has no server to dial",
                )))
            })
        });
        Self::build(connection, dial, smtp, None, None)
    }

    /// A provider bound to `mailbox` for its email scope, drawing its connections from this
    /// account's pool. Dials nothing.
    #[must_use]
    pub fn provider(&self, mailbox: MailboxId) -> ImapProvider<S> {
        ImapProvider::new(
            Arc::clone(&self.pool),
            mailbox,
            self.smtp.clone(),
            self.since,
            self.connection_info,
        )
    }

    /// How many more folders this account can watch before a watch would take the last
    /// connection syncing needs. A host reads this to warn *before* offering a choice that
    /// [`watch`](Self::watch) would refuse.
    #[must_use]
    pub fn watch_headroom(&self) -> usize {
        self.pool.watch_headroom()
    }

    /// Drops every resting connection, so the next call dials fresh — for a host that has seen
    /// the network change. The sockets look open and are not, and proving that one at a time
    /// costs a timeout apiece. Connections in use finish their call and are then closed; open
    /// watches are untouched, and fail on their own when their socket does.
    pub fn invalidate(&self) {
        self.pool.invalidate();
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ImapAccount<S> {
    /// Opens a standing `IDLE` watch on `mailbox`, with the given `keepalive` (see
    /// [`DEFAULT_IDLE_KEEPALIVE`](crate::DEFAULT_IDLE_KEEPALIVE); a host value is clamped to a
    /// sane range).
    ///
    /// A watch holds its connection for as long as it lives — a connection in `IDLE` cannot
    /// serve anything else — so it spends one of the account's connections until the watcher
    /// is dropped, and the rest of the budget syncs.
    ///
    /// # Errors
    ///
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) when the
    /// server does not advertise `IDLE`, or when this watch would leave the account no
    /// connection to sync with ([`watch_headroom`](Self::watch_headroom) is `0`) — either way,
    /// the host should poll this folder instead. The classified dial or `EXAMINE` failure
    /// otherwise.
    pub async fn watch(
        &self,
        mailbox: MailboxId,
        keepalive: Duration,
    ) -> ProviderResult<ImapWatcher<S>> {
        let (connection, lease) = self.pool.reserve_watch().await?;
        let watcher = ImapWatcher::start(connection, mailbox, keepalive).await?;
        Ok(watcher.with_lease(lease))
    }
}

/// What every provider of an account advertises: the mail surface every IMAP session has,
/// submission when SMTP is configured, and push when the server offers `IDLE`.
fn capabilities<S>(connection: &Connection<S>, smtp: bool) -> Capabilities {
    // Mail writes (`UID STORE`/`MOVE`/`EXPUNGE`) and body fetch (`UID FETCH BODY.PEEK[]`) need
    // no extra config — every IMAP session can issue them — so those capabilities are
    // unconditional, unlike submission which depends on a configured SMTP transport.
    let mut capabilities = Capabilities::none()
        .with_mail()
        .with_mail_writes()
        // All three registered keywords are expressible; whether the *server* stores them is a
        // per-mailbox fact (`\*` in `PERMANENTFLAGS`) that only a `SELECT` can answer, so the
        // report path checks it per call and refuses rather than writing a flag the server
        // discards (`crate::report`). The evidence is `Convention`: IMAP has no way to say
        // whether anything trained on the keyword.
        .with_mail_report(ReportControls {
            verdicts: ReportVerdicts::all(),
            evidence: ReportEvidence::Convention,
        })
        .with_message_source()
        // Storing a draft is an `APPEND`, so it needs nothing submission needs: an account with
        // no SMTP transport configured can still keep drafts.
        .with_mail_drafts();
    if smtp {
        // Both submission capabilities ride the same SMTP transport: the assembler
        // (`engine-rfc5322`) builds the whole message, so this adapter owns every
        // `Content-Type` parameter — including the `method=` that makes an iTIP object a
        // scheduling message rather than a calendar file (RFC 6047 §2.4). Contrast JMAP, which
        // hands the server a body structure and cannot.
        capabilities = capabilities.with_submission().with_scheduling_submission();
    }
    // Push (`IDLE`, RFC 2177) is gated on the server advertising it post-auth, so a host knows
    // whether to offer an "as it comes in" strategy or fall back to polling.
    if connection.idle_available() {
        capabilities = capabilities.with_idle();
    }
    capabilities
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;
