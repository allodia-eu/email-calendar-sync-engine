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
    ids::{AccountId, MailboxId, ProviderKey, SharedMailboxId},
    mail::{Mailbox, Message},
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    CalendarWrites, Capabilities, ConnectionInfo, Draft, EmailStream, MailEdit, MailEditReceipt,
    MessageReport, Provider, ProviderError, ProviderResult, ReportControls, ReportEvidence,
    ReportReceipt, ReportVerdicts, ScopeSync, SharedMailbox, SharedMailboxes, SubmissionReceipt,
    TlsVersion,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::{
    capability::Extension,
    config::ImapConfig,
    connect::connect_session,
    error::ImapError,
    filing::{Redial, SmtpSender, resolve_smtp},
    store::Namespaces,
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
    /// The shared mail store this provider covers (`ImapConfig::with_shared_mailbox`), or
    /// `None` for the credential's own. `pub(crate)` for `crate::folders`, which turns it
    /// into the store every folder list and placement is scoped to.
    pub(crate) shared_mailbox: Option<SharedMailboxId>,
    /// The server's `NAMESPACE` answer, learned the first time something needs it
    /// (`crate::folders`) — never at connect, which one provider per folder would pay for
    /// nothing.
    pub(crate) namespaces: std::sync::OnceLock<Namespaces>,
}

impl<S> core::fmt::Debug for ImapProvider<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapProvider")
            .field("mailbox", &self.mailbox)
            .field("since", &self.since)
            .field("connection_info", &self.connection_info)
            .field("shared_mailbox", &self.shared_mailbox)
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
        let mut provider = Self::build(connection, mailbox, smtp, config.since, tls_version);
        // Only a provider that dialed knows how to dial again — which is what lets a Sent
        // copy that fails to file over this session be retried on a fresh one.
        provider.redial = Some(Redial::new(config, &connector));
        provider.shared_mailbox.clone_from(&config.shared_mailbox);
        Ok(provider)
    }
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
            // All three registered keywords are expressible; whether the *server* stores
            // them is a per-mailbox fact (`\*` in `PERMANENTFLAGS`) that only a `SELECT`
            // can answer, so the report path checks it per call and refuses rather than
            // writing a flag the server discards (`crate::report`). The evidence is
            // `Convention`: IMAP has no way to say whether anything trained on the keyword.
            .with_mail_report(ReportControls {
                verdicts: ReportVerdicts::all(),
                evidence: ReportEvidence::Convention,
            })
            .with_message_source()
            // Storing a draft is an `APPEND`, so it needs nothing submission needs: an
            // account with no SMTP transport configured can still keep drafts.
            .with_mail_drafts();
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
        // Shares are listable where the server can say whose mail is whose (`NAMESPACE`)
        // and can grant access at all (`ACL`, RFC 4314 — how one IMAP user shares with
        // another). Gmail and Outlook.com advertise `NAMESPACE` alone; claiming a list
        // there would offer a picker that can never fill. Unlike the folder list, this is
        // read off the negotiation, so it costs no command (`crate::folders`).
        if connection.negotiated.has(Extension::Namespace)
            && connection.negotiated.has(Extension::Acl)
        {
            capabilities = capabilities.with_shared_mailboxes(SharedMailboxes::Enumerable);
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
            shared_mailbox: None,
            namespaces: std::sync::OnceLock::new(),
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

    /// The folders of the store this provider covers — the credential's own, or the shared
    /// mailbox it is bound to — never every folder the credential can see: a flat `LIST`
    /// returns both, and one account must hold one principal's mail (`crate::store`). Each
    /// carries its unread count and the caller's rights (`crate::folders`).
    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let mailboxes = {
            let mut connection = self.connection.lock().await;
            let store = self.store_on(&mut connection).await?;
            crate::folders::list_store(&mut connection, &store).await?
        };
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
        let stream: EmailStream<'a> = Box::pin(crate::stream::stream_email(
            &self.connection,
            &self.mailbox,
            cursor,
            window,
            fetch_batch,
            chunk_size,
        ));
        if self.shared_mailbox.is_none() {
            return stream;
        }
        // A provider scoped to a shared store syncs that store's folders only. Binding any
        // other folder to it would put that folder's mail in the shared mailbox's account —
        // refused, not synced.
        let checked = async move {
            match self.check_in_store(&self.mailbox).await {
                Ok(()) => stream,
                Err(refusal) => Box::pin(futures_util::stream::once(async move { Err(refusal) })),
            }
        };
        Box::pin(futures_util::StreamExt::flatten(
            futures_util::stream::once(checked),
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

    /// Stores a draft in the account's `\\Drafts` folder (`APPEND`), removing the copy it
    /// supersedes.
    ///
    /// A thin lock-and-call, like [`Provider::edit_mail`]: the ordering rule that decides
    /// whether a partial failure duplicates a draft or loses one lives in `crate::drafts`,
    /// so it stays stream-generic and unit-testable.
    async fn put_draft(
        &self,
        _account: &AccountId,
        draft: &Draft,
        replacing: Option<&ProviderKey>,
    ) -> ProviderResult<ProviderKey> {
        let mut connection = self.connection.lock().await;
        let store = self.store_on(&mut connection).await?;
        crate::drafts::put_draft(&mut connection, &store, draft, replacing).await
    }

    /// Removes a stored draft (`UID STORE \\Deleted` + `UID EXPUNGE`), reporting one that
    /// is already gone as done.
    async fn delete_draft(&self, _account: &AccountId, draft: &ProviderKey) -> ProviderResult<()> {
        let mut connection = self.connection.lock().await;
        let store = self.store_on(&mut connection).await?;
        crate::drafts::delete_draft(&mut connection, &store, draft).await
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

    /// The stores this credential can open besides its own: one per owner under each
    /// foreign namespace (`crate::folders`). Its own store is never among them — its
    /// prefix is the empty string, which is no handle — and the answer is the same
    /// whichever store this provider covers. Resolving an address reads this list through
    /// the trait's default.
    async fn list_shared_mailboxes(&self) -> ProviderResult<Vec<SharedMailbox>> {
        if self.connection_info.capabilities.shared_mailboxes() != SharedMailboxes::Enumerable {
            return Err(ProviderError::invalid_state(
                "provider does not support listing shared mailboxes",
            ));
        }
        let mut connection = self.connection.lock().await;
        let namespaces = self.namespaces_on(&mut connection).await?;
        crate::folders::discover(&mut connection, &namespaces).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> CalendarWrites for ImapProvider<S> {}

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
