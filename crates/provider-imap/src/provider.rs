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
    Capabilities, ConnectionInfo, Draft, EmailStream, MailEdit, MailEditReceipt, MessageReport,
    Provider, ProviderResult, ReportControls, ReportEvidence, ReportReceipt, ReportVerdicts,
    ScopeSync, SubmissionReceipt, TlsVersion,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::{
    config::ImapConfig,
    dial::connect_session,
    error::ImapError,
    filing::{Redial, SmtpSender, resolve_smtp},
    mail::mailbox_from_list,
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
    /// What a fresh IMAP session needs, so a Sent copy that fails to file over the
    /// standing session above is retried on a new one rather than lost. `None` for a
    /// provider built over a mock stream, which has no server to re-dial.
    pub(crate) redial: Option<Redial>,
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
    /// Connects over implicit TLS, authenticates with the config's
    /// [`Credentials`](crate::Credentials) — a password or an OAuth 2.0 access token —
    /// and binds `mailbox` for the email scope.
    ///
    /// The `connector` carries the host's trust policy — the library never bakes in
    /// a root store, so a mobile host (or the self-signed test fixture) injects its
    /// own (`docs/agent-guidance/imap-smtp.md`).
    ///
    /// # Errors
    ///
    /// [`ImapError`] on a TCP/TLS/authentication failure or a bad server name.
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
        let mut provider = Self::build(connection, mailbox, smtp, config.since, tls_version);
        // Only a provider that dialed knows how to dial again — which is what lets a Sent
        // copy that fails to file over this session be retried on a fresh one.
        provider.redial = Some(Redial::new(config, &connector));
        Ok(provider)
    }
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
            // All three registered keywords are expressible; whether the *server* stores
            // them is a per-mailbox fact (`\*` in `PERMANENTFLAGS`) that only a `SELECT`
            // can answer, so the report path checks it per call and refuses rather than
            // writing a flag the server discards (`crate::report`). The evidence is
            // `Convention`: IMAP has no way to say whether anything trained on the keyword.
            .with_mail_report(ReportControls {
                verdicts: ReportVerdicts::all(),
                evidence: ReportEvidence::Convention,
            })
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
        if connection.idle_available() {
            capabilities = capabilities.with_idle();
        }
        Self {
            connection: Mutex::new(connection),
            mailbox,
            smtp,
            redial: None,
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
        let (modified_utf7, rows, unseen) = {
            let mut connection = self.connection.lock().await;
            // The dialect decides how the names in these rows are encoded, and it is a
            // property of the session, so it is read under the same lock as the rows.
            let modified_utf7 = connection.names_are_modified_utf7();
            let (rows, unseen) = if connection
                .negotiated
                .has(crate::capability::Extension::ListStatus)
            {
                connection.list_with_unseen().await?
            } else {
                let rows = connection.list().await?;
                let unseen = crate::unseen::unseen_by_probing(&mut connection, &rows).await?;
                (rows, unseen)
            };
            (modified_utf7, rows, unseen)
        };
        let mailboxes: Vec<Mailbox> = rows
            .iter()
            .filter_map(|row| {
                let mut mailbox = mailbox_from_list(row, modified_utf7)?;
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

    /// Files the Sent copy of an already-delivered message, for a host repairing a
    /// submission that came back `Unfiled` (`crate::filing`). Idempotent: it probes for the
    /// copy before placing one, on the standing session and on a freshly dialed retry.
    async fn file_sent_copy(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<ProviderKey> {
        self.refile(draft).await
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

    /// Reports a message as junk / not junk / phishing.
    ///
    /// A thin lock-and-call, like [`Provider::edit_mail`]: the keyword choice, the
    /// `PERMANENTFLAGS` check and the move live in `crate::report` so they stay
    /// stream-generic and unit-testable.
    async fn report_message(
        &self,
        _account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        let mut connection = self.connection.lock().await;
        crate::report::report_message(&mut connection, report).await
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
