//! The [`Provider`] implementation: an [`ImapProvider`] bound to one mailbox for
//! email, syncing the account's folder list under the per-account
//! [`SyncScope::ImapMailboxList`].
//!
//! The connection is stateful (one TLS socket, sequential commands), so it is held
//! behind an async [`Mutex`] — concurrent `stream_email` calls serialize onto
//! the one IMAP session, which is exactly IMAP's model. Method execution is generic
//! over the stream, so the offline tests drive the full `Provider` surface over a
//! mock while [`ImapProvider::connect`] uses a `tokio-rustls` TLS stream.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Mailbox, Message},
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    Capabilities, ConnectObserver, ConnectStep, ConnectionInfo, Draft, EmailStream, MailEdit,
    MailEditReceipt, Provider, ProviderResult, ScopeSync, SubmissionReceipt, TlsVersion,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::{
    config::{ImapConfig, ImapSecurity},
    error::ImapError,
    filing::{SmtpSender, resolve_smtp},
    mail::mailbox_from_list,
    tls_info,
    transport::Connection,
};

/// The IMAP folder list carries no sync token (a `LIST` re-snapshots it each pass),
/// so its cursor is a fixed sentinel — the store round-trips it unread.
const FOLDER_LIST_CURSOR: &str = "imap-folders";

/// An IMAP read/sync provider bound to a single mailbox for its email scope, with
/// optional SMTP submission.
pub struct ImapProvider<S> {
    /// `pub(crate)` so the [`crate::filing`] submission/draft helpers (split out to
    /// keep this file under the size limit) can lock the shared IMAP session.
    pub(crate) connection: Mutex<Connection<S>>,
    mailbox: MailboxId,
    /// The resolved SMTP transport, or `None` when submission is unconfigured.
    /// `pub(crate)` so [`crate::filing`] (which owns the submission dispatch) reads it.
    pub(crate) smtp: Option<SmtpSender>,
    /// The sync-depth window floor: when set, a snapshot fetches only mail delivered
    /// on or after this date (`ImapConfig::with_since`). `None` syncs the whole mailbox.
    since: Option<time::Date>,
    /// The post-connect facts: the capabilities negotiated post-auth, and the TLS
    /// version of the **IMAP** session. SMTP submission re-dials per send
    /// (`SmtpSender::ImplicitTls`), so its handshake is not a fact of this provider.
    connection_info: ConnectionInfo,
}

impl<S> core::fmt::Debug for ImapProvider<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapProvider")
            .field("mailbox", &self.mailbox)
            .field("since", &self.since)
            .field("connection_info", &self.connection_info)
            .finish_non_exhaustive()
    }
}

impl ImapProvider<TlsStream<TcpStream>> {
    /// Connects over implicit TLS, logs in, and binds `mailbox` for the email scope.
    ///
    /// The `connector` carries the host's trust policy — the library never bakes in
    /// a root store, so a mobile host (or the self-signed test fixture) injects its
    /// own (`docs/agent-guidance/imap-smtp.md`).
    ///
    /// # Errors
    ///
    /// [`ImapError`] on a TCP/TLS/login failure or a bad server name.
    pub async fn connect(
        config: &ImapConfig,
        connector: TlsConnector,
        mailbox: MailboxId,
    ) -> Result<Self, ImapError> {
        // Resolve the SMTP sender first (cloning the connector), so SMTP-over-TLS can
        // re-dial with the host's trust policy after the IMAP connect consumes it.
        let smtp = config
            .smtp
            .as_ref()
            .map(|settings| resolve_smtp(settings, &connector, config));
        let (connection, tls_version) = connect_session(config, &connector).await?;
        Ok(Self::build(
            connection,
            mailbox,
            smtp,
            config.since,
            tls_version,
        ))
    }
}

/// Opens a TCP + implicit-TLS connection, logs in, and negotiates capabilities
/// (ENABLE QRESYNC + record IDLE) — the shared dial both [`ImapProvider::connect`]
/// and [`ImapWatcher::connect`](crate::watch::ImapWatcher::connect) build their session
/// on. Factored out so a watcher opens its **own** dedicated connection (push needs a
/// standing IDLE socket separate from the sync socket) without duplicating the
/// connect/login/negotiate sequence or exposing the config's private fields.
///
/// Returns the session together with the TLS version its handshake agreed — the one
/// point where the concrete stream type is still visible, before it is erased behind
/// the generic [`Connection<S>`] (`tls_info`).
///
/// # Errors
///
/// [`ImapError`] on a TCP/TLS/login failure or a bad server name.
pub(crate) async fn connect_session(
    config: &ImapConfig,
    connector: &TlsConnector,
) -> Result<(Connection<TlsStream<TcpStream>>, Option<TlsVersion>), ImapError> {
    let tcp = TcpStream::connect(&config.addr).await?;
    let server_name = ServerName::try_from(config.server_name.clone())
        .map_err(|e| ImapError::bad(format!("invalid TLS server name: {e}")))?;
    // Implicit TLS wraps the socket now; STARTTLS runs the plaintext handshake command
    // first and upgrades the raw socket in place. Either way the result is one
    // `TlsStream`, so the rest of the dial — and every downstream type — is identical.
    let (tls, resumed) = match config.security {
        ImapSecurity::ImplicitTls => (connector.connect(server_name, tcp).await?, false),
        ImapSecurity::StartTls => {
            let mut plain = Connection::open(tcp).await?;
            plain.start_tls().await?;
            let upgraded = connector
                .connect(server_name, plain.into_inner_stream()?)
                .await?;
            (upgraded, true)
        }
    };
    let tls_version = tls_info::tls_version(&tls);
    // STARTTLS already consumed the one greeting on the plaintext connection, so it
    // resumes without reading another; implicit TLS reads its greeting here.
    let connection = if resumed {
        finish_session(Connection::resume(tls), tls_version, config).await?
    } else {
        open_session(tls, tls_version, config).await?
    };
    Ok((connection, tls_version))
}

/// Greets over an already-established `stream`, then logs in and negotiates
/// capabilities via [`finish_session`]. The implicit-TLS / mock path (the STARTTLS
/// path uses [`finish_session`] directly, its greeting already read).
///
/// Generic over the stream, so the offline suite asserts the exact step sequence over
/// a `MockStream` — the handshake has already happened by the time this is called, so
/// passing `tls_version` in keeps the emitted order identical to the live dial's.
///
/// # Errors
///
/// [`ImapError`] on a greeting, login, or capability-negotiation failure.
pub(crate) async fn open_session<S: AsyncRead + AsyncWrite + Unpin + Send>(
    stream: S,
    tls_version: Option<TlsVersion>,
    config: &ImapConfig,
) -> Result<Connection<S>, ImapError> {
    finish_session(Connection::open(stream).await?, tls_version, config).await
}

/// Logs in and negotiates capabilities over an already-greeted `connection`, reporting
/// [`ConnectStep::TlsEstablished`] (when the handshake agreed a version) then
/// [`ConnectStep::Authenticated`] to the config's observer. Shared by the implicit-TLS
/// path ([`open_session`]) and the STARTTLS resume, so both emit the same step order.
///
/// # Errors
///
/// [`ImapError`] on a login or capability-negotiation failure.
async fn finish_session<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut connection: Connection<S>,
    tls_version: Option<TlsVersion>,
    config: &ImapConfig,
) -> Result<Connection<S>, ImapError> {
    let observer: &dyn ConnectObserver = config.connect_observer();
    if let Some(version) = tls_version {
        observer.step(&ConnectStep::TlsEstablished(version));
    }
    connection.login(&config.username, &config.password).await?;
    observer.step(&ConnectStep::Authenticated);
    // Detect + ENABLE QRESYNC (RFC 7162) so deltas reconcile flag/expunge changes
    // incrementally, and record IDLE (RFC 2177) support; a server without either stays
    // on the corresponding baseline. Not a connect step: it negotiates extensions, it
    // does not establish the connection.
    connection.negotiate_qresync().await?;
    Ok(connection)
}

/// Formats a calendar date as the IMAP `d-Mon-yyyy` form `UID SEARCH SINCE` expects
/// (RFC 9051 §6.4.4), e.g. 2026-03-18 → `18-Mar-2026`. The month is a fixed English
/// abbreviation and the rest is digits, so the result is a safe, unquoted search atom.
pub(crate) fn format_imap_date(date: time::Date) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS[usize::from(u8::from(date.month())) - 1];
    format!("{}-{month}-{}", date.day(), date.year())
}

impl<S> ImapProvider<S> {
    /// Builds a provider, advertising submission iff SMTP is configured, and recording
    /// the `tls_version` its dial negotiated (`None` when the stream is not TLS — the
    /// offline mock).
    fn build(
        connection: Connection<S>,
        mailbox: MailboxId,
        smtp: Option<SmtpSender>,
        since: Option<time::Date>,
        tls_version: Option<TlsVersion>,
    ) -> Self {
        // Mail writes (`UID STORE`/`MOVE`/`EXPUNGE`) and body fetch (`UID FETCH
        // BODY.PEEK[]`) need no extra config — every IMAP session can issue them — so
        // those capabilities are unconditional, unlike submission which depends on a
        // configured SMTP transport.
        let mut capabilities = Capabilities::none()
            .with_mail()
            .with_mail_writes()
            .with_message_source();
        if smtp.is_some() {
            // Both submission capabilities ride the same SMTP transport: the assembler
            // (`engine-rfc5322`) builds the whole message, so this adapter owns every
            // `Content-Type` parameter — including the `method=` that makes an iTIP object a
            // scheduling message rather than a calendar file (RFC 6047 §2.4). Contrast JMAP,
            // which hands the server a body structure and cannot.
            capabilities = capabilities.with_submission().with_scheduling_submission();
        }
        // Push (`IDLE`, RFC 2177) is gated on the server advertising it post-auth, so a
        // host knows whether to offer an "as it comes in" strategy or fall back to
        // polling. The watcher itself opens a *separate* connection (`crate::watch`).
        if connection.idle_advertised() {
            capabilities = capabilities.with_idle();
        }
        Self {
            connection: Mutex::new(connection),
            mailbox,
            smtp,
            since,
            connection_info: ConnectionInfo {
                tls_version,
                ..ConnectionInfo::new(capabilities)
            },
        }
    }

    /// Wraps an already-open, logged-in connection bound to `mailbox` (mail only).
    /// Offline tests use this over a mock stream; the live path is
    /// [`ImapProvider::connect`].
    #[cfg(test)]
    pub(crate) fn with_connection(connection: Connection<S>, mailbox: MailboxId) -> Self {
        Self::build(connection, mailbox, None, None, None)
    }

    /// Wraps a mock IMAP `connection` but with an injected `smtp` sender, so the
    /// offline suite can drive [`submit`](Self::submit) against an in-process SMTP
    /// server (the IMAP filing side degrades gracefully over the exhausted mock).
    #[cfg(test)]
    pub(crate) fn with_connection_and_smtp(
        connection: Connection<S>,
        mailbox: MailboxId,
        smtp: SmtpSender,
    ) -> Self {
        Self::build(connection, mailbox, Some(smtp), None, None)
    }
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> Provider for ImapProvider<S> {
    fn connection_info(&self) -> ConnectionInfo {
        self.connection_info
    }

    /// IMAP folder-list state is per account, so the mailbox container syncs under
    /// [`SyncScope::ImapMailboxList`] — distinct from any one mailbox's email scope.
    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailboxList {
            account: account.clone(),
        }
    }

    /// IMAP email state is per mailbox, so this provider's email scope names its
    /// bound mailbox.
    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        // `LIST` alone carries no unread count, so the folder list either asks for it
        // in the same round trip (LIST-STATUS) or probes each mailbox afterwards —
        // `unseen` owns that choice and its cost.
        let (rows, unseen) = {
            let mut connection = self.connection.lock().await;
            if connection.list_status_advertised {
                connection.list_with_unseen().await?
            } else {
                let rows = connection.list().await?;
                let unseen = crate::unseen::unseen_by_probing(&mut connection, &rows).await?;
                (rows, unseen)
            }
        };
        let mailboxes: Vec<Mailbox> = rows
            .iter()
            .filter_map(|row| {
                let mut mailbox = mailbox_from_list(row)?;
                // Absent stays absent: a mailbox the server did not count must not
                // read as one with nothing unread.
                mailbox.unread_count = unseen.get(&row.name).copied();
                Some(mailbox)
            })
            .collect();
        // `LIST` is a full snapshot every pass, so every folder is `present`.
        let present: BTreeSet<ProviderKey> = mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(mailboxes, present),
            SyncState::new(FOLDER_LIST_CURSOR),
        ))
    }

    /// The whole-scope drain windows under the construction cutoff (`with_since`);
    /// the streaming path takes its window explicitly, so a host changes depth per
    /// sync without reconnecting.
    fn default_sync_window(&self) -> SyncWindow {
        self.since.map_or_else(SyncWindow::full, |date| {
            // A construction cutoff is always a real date, so this conversion holds.
            CalendarDate::new(date.year(), u8::from(date.month()), date.day())
                .map_or_else(|_| SyncWindow::full(), SyncWindow::since)
        })
    }

    /// Streams the bound mailbox's email incrementally and resumably (see the
    /// `stream` module): rows parse off the wire so a chunk commits sub-batch, and a
    /// cold backfill checkpoints its low-UID watermark so a kill resumes where it
    /// stopped. `fetch_batch` bounds each `UID FETCH` group; `chunk_size` the commit
    /// granularity.
    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        // An unbounded caller window falls back to the construction cutoff
        // (`with_since`), so a provider built with a depth still windows its streamed
        // backfill; an explicit per-sync window overrides it.
        let window = if window.is_bounded() {
            window
        } else {
            self.default_sync_window()
        };
        Box::pin(crate::stream::stream_email(
            &self.connection,
            &self.mailbox,
            cursor,
            window,
            fetch_batch,
            chunk_size,
        ))
    }

    /// Submits `draft` over the configured SMTP transport and files the sent copy in
    /// Sent (`crate::filing`). Plaintext, implicit TLS, and STARTTLS are all dispatched
    /// there; `AUTH PLAIN` runs only once the stream is TLS-secured.
    ///
    /// The pre-generated `Message-ID` travels on the message, so the sent copy
    /// reconciles by it. A post-`DATA` ambiguity becomes a
    /// [`ProviderError::needs_confirmation`](engine_provider::ProviderError::needs_confirmation)
    /// (never blind-retried); a clean rejection is permanent (5xx) or transient (4xx). Sent
    /// placement is a best-effort `APPEND` — a successful send is not failed for a Sent-filing
    /// hiccup; with UIDPLUS the receipt carries the real Sent key, otherwise a
    /// `Message-ID`-derived one that the next Sent sync resolves.
    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        self.submit(draft).await
    }

    /// Applies a [`MailEdit`] to the bound mailbox: mark-read/flag (`UID STORE`),
    /// move (`UID MOVE`), or permanent delete (`UID STORE \Deleted` + `UID EXPUNGE`).
    ///
    /// A thin lock-and-call: the mutation logic (key parse, the SELECT + UIDVALIDITY
    /// guard, command dispatch) lives in the `mutate` module so it stays
    /// stream-generic and unit-testable. A stale UID (its mailbox's `UIDVALIDITY`
    /// changed) is a [`ProviderError::conflict`](engine_provider::ProviderError::conflict).
    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        let mut connection = self.connection.lock().await;
        crate::mutate::edit_mail(&mut connection, edit).await
    }

    /// Fetches a message's raw RFC 5322 source (`UID FETCH BODY.PEEK[]`).
    ///
    /// A thin lock-and-call: the fetch logic (key parse, the SELECT + UIDVALIDITY
    /// guard, the body read) lives in the `fetch` module so it stays stream-generic
    /// and unit-testable. The message is addressed by its own key, so any of the
    /// account's folders can be read over this one bound session; a stale UID (its
    /// mailbox's `UIDVALIDITY` changed) is a
    /// [`ProviderError::conflict`](engine_provider::ProviderError::conflict).
    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<engine_core::raw::RawMime> {
        let mut connection = self.connection.lock().await;
        crate::fetch::fetch_message_source(&mut connection, message.id.key()).await
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

// The `submit_over` submission tests live in a sibling file so `provider_tests.rs`
// stays under the line limit.
#[cfg(test)]
#[path = "provider_submit_over_tests.rs"]
mod submit_over_tests;

// The STARTTLS connect path drives a real in-process TLS server, so it lives in its own
// sibling file (with its own cert/server harness).
#[cfg(test)]
#[path = "provider_starttls_tests.rs"]
mod starttls_tests;
