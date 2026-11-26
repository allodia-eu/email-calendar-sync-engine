//! The [`Provider`] implementation: an [`ImapProvider`] bound to one mailbox for
//! email, syncing the account's folder list under the per-account
//! [`SyncScope::ImapMailboxList`].
//!
//! A provider owns no connection. Each call borrows one from its account's pool
//! ([`crate::pool`]) for exactly as long as the call runs — a streamed pass for as long as the
//! stream lives — so a provider that is not syncing holds no socket, and concurrent calls run
//! on separate connections up to the account's budget and wait beyond it. Method execution is
//! generic over the stream, so the offline tests drive the full `Provider` surface over a mock
//! while [`ImapAccount::connect`](crate::ImapAccount::connect) uses a `tokio-rustls` TLS
//! stream.

use std::{
    collections::BTreeSet,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey, SharedAccountId},
    mail::{Mailbox, Message, MessageMetadata},
    sync::{MailboxWindows, SyncScope, SyncState, SyncUpdate, SyncWindow, Widening},
    time::CalendarDate,
};
use engine_provider::{
    CalendarSync, CalendarWrites, ConnectionInfo, Draft, DraftReceipt, EmailStream, MailEdit,
    MailEditReceipt, MailboxEdit, MailboxEditReceipt, MailboxWrites, MessageReport, Provider,
    ProviderError, ProviderResult, ReportReceipt, ScopeSync, SharedAccount, SharedAccounts,
    SourceStream, SubmissionReceipt,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    filing::SmtpSender,
    managesieve::Sieve,
    pool::{ImapPool, PooledConnection},
    store::Namespaces,
};

/// The IMAP folder list carries no sync token (a `LIST` re-snapshots it each pass),
/// so its cursor is a fixed sentinel — the store round-trips it unread.
const FOLDER_LIST_CURSOR: &str = "imap-folders";

/// An IMAP read/sync provider bound to a single mailbox for its email scope, with
/// optional SMTP submission. Made by [`ImapAccount::provider`](crate::ImapAccount::provider).
pub struct ImapProvider<S> {
    /// The account's connections, shared with every other folder's provider and watcher.
    /// `pub(crate)` so the [`crate::filing`] submission/draft helpers (split out to keep this
    /// file under the size limit) can borrow a session.
    pub(crate) pool: Arc<ImapPool<S>>,
    pub(crate) mailbox: MailboxId,
    /// The resolved SMTP transport, or `None` when submission is unconfigured.
    /// `pub(crate)` so [`crate::filing`] (which owns the submission dispatch) reads it.
    pub(crate) smtp: Option<Arc<SmtpSender>>,
    /// The sync-depth window floor: when set, a snapshot fetches only mail delivered
    /// on or after this date (`ImapConfig::with_since`). `None` syncs the whole mailbox.
    since: Option<time::Date>,
    /// The post-connect facts: the capabilities negotiated post-auth, and the TLS
    /// version of the **IMAP** session. SMTP submission re-dials per send
    /// (`SmtpSender::ImplicitTls`), so its handshake is not a fact of this provider.
    connection_info: ConnectionInfo,
    /// The shared mail store this provider covers (`ImapAccount::with_shared_account`), or
    /// `None` for the credential's own. `pub(crate)` for `crate::folders`, which turns it
    /// into the store every folder list and placement is scoped to.
    pub(crate) shared_account: Option<SharedAccountId>,
    /// The server's `NAMESPACE` answer, learned the first time something needs it
    /// (`crate::folders`) — never at connect, which most providers would pay for nothing —
    /// and shared by every provider of the account, so the login asks once.
    pub(crate) namespaces: Arc<OnceLock<Namespaces>>,
    /// The account's rules and out-of-office reply over ManageSieve, where the host configured it
    /// and it answered when the account connected (`crate::managesieve`).
    sieve: Option<Arc<dyn Sieve>>,
}

impl<S> core::fmt::Debug for ImapProvider<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapProvider")
            .field("mailbox", &self.mailbox)
            .field("since", &self.since)
            .field("connection_info", &self.connection_info)
            .field("shared_account", &self.shared_account)
            .finish_non_exhaustive()
    }
}

impl<S> ImapProvider<S> {
    /// A provider bound to `mailbox`, borrowing from `pool`. The account computes the rest once
    /// for all of its providers (`crate::account`).
    pub(crate) fn new(
        pool: Arc<ImapPool<S>>,
        mailbox: MailboxId,
        smtp: Option<Arc<SmtpSender>>,
        since: Option<time::Date>,
        connection_info: ConnectionInfo,
        shared_account: Option<SharedAccountId>,
        namespaces: Arc<OnceLock<Namespaces>>,
    ) -> Self {
        Self {
            pool,
            mailbox,
            smtp,
            since,
            connection_info,
            shared_account,
            namespaces,
            sieve: None,
        }
    }

    /// Offers the account's rules and reply through `sieve` (builder-style).
    pub(crate) fn with_sieve(mut self, sieve: Option<Arc<dyn Sieve>>) -> Self {
        self.sieve = sieve;
        self
    }

    /// Wraps an already-open, logged-in connection bound to `mailbox` (mail only), in an
    /// account of its own that cannot dial another. Offline tests use this over a mock stream;
    /// the live path is [`ImapAccount::connect`](crate::ImapAccount::connect).
    #[cfg(test)]
    pub(crate) fn with_connection(
        connection: crate::transport::Connection<S>,
        mailbox: MailboxId,
    ) -> Self
    where
        S: 'static,
    {
        crate::ImapAccount::offline(connection, None).provider(mailbox)
    }

    /// Wraps a mock IMAP `connection` but with an injected `smtp` sender, so the
    /// offline suite can drive [`submit`](Self::submit) against an in-process SMTP
    /// server (the IMAP filing side degrades gracefully over the exhausted mock).
    #[cfg(test)]
    pub(crate) fn with_connection_and_smtp(
        connection: crate::transport::Connection<S>,
        mailbox: MailboxId,
        smtp: SmtpSender,
    ) -> Self
    where
        S: 'static,
    {
        crate::ImapAccount::offline(connection, Some(smtp)).provider(mailbox)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ImapProvider<S> {
    /// Borrows a connection for one call. `pub(crate)` for the same reason as `pool`.
    ///
    /// # Errors
    ///
    /// The classified dial failure, when nothing is parked and a new connection cannot open.
    pub(crate) async fn session(&self) -> ProviderResult<PooledConnection<S>> {
        Ok(self.pool.acquire().await?)
    }
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> Provider for ImapProvider<S> {
    /// IMAP imports by `APPEND` (`crate::import`), so a host holding this adapter as a
    /// `dyn Provider` reaches it here.
    #[cfg(feature = "import")]
    fn mail_importer(&self) -> Option<&dyn engine_provider::import::MailImporter> {
        Some(self)
    }

    /// Over ManageSieve, where the host configured it ([`ImapConfig::with_managesieve`]) and the
    /// last probe of its greeting succeeded (at connect, or beside a later dial after a failure
    /// that may pass or `ImapAccount::invalidate`); never for a shared mail store, whose rules are
    /// its owner's. `Some` says the service answered, not that signing in works: no credential
    /// was presented, so a credential ManageSieve does not carry, or a token where the server
    /// offers only `PLAIN`, fails at the first call.
    ///
    /// [`ImapConfig::with_managesieve`]: crate::ImapConfig::with_managesieve
    fn mail_rules(&self) -> Option<&dyn engine_provider::MailRules> {
        self.sieve
            .as_deref()
            .filter(|sieve| sieve.offered().is_some())
            .map(|sieve| sieve as &dyn engine_provider::MailRules)
    }

    /// As [`mail_rules`](Self::mail_rules), where the server also listed `vacation`: the reply is
    /// a rule of the same script, sharing its version. The same caveat on signing in holds.
    fn vacation(&self) -> Option<&dyn engine_provider::Vacation> {
        self.sieve
            .as_deref()
            .filter(|sieve| sieve.offered() == Some(true))
            .map(|sieve| sieve as &dyn engine_provider::Vacation)
    }

    /// As [`mail_rules`](Self::mail_rules): rules and the reply imported over the same script.
    #[cfg(feature = "import")]
    fn rules_importer(&self) -> Option<&dyn engine_provider::import::RulesImporter> {
        self.sieve
            .as_deref()
            .filter(|sieve| sieve.offered().is_some())
            .map(crate::managesieve::Sieve::importer)
    }

    /// The negotiated facts, and as many overlapping body fetches as the account has worker
    /// connections for right now: each fetch borrows its own from the pool, so the width is the
    /// pool's, less the connections its watches hold. Read per call, because it moves as
    /// watches start and stop and when a server's refusal lowers the ceiling.
    fn connection_info(&self) -> ConnectionInfo {
        self.connection_info
            .with_concurrent_fetches(self.pool.worker_capacity())
            .with_sources_per_request(crate::fetch_batch::SOURCES_PER_REQUEST)
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
        let mut connection = self.session().await?;
        let listed = match self.store_on(&mut connection).await {
            Ok(store) => crate::folders::list_store(&mut connection, &store).await,
            Err(err) => Err(err),
        };
        let mailboxes = connection.settle(listed)?;
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
        window: MailboxWindows,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        // The bound mailbox fetches under its own window. An unbounded one falls back to the
        // construction cutoff (`with_since`), so a provider built with a depth still windows
        // its streamed backfill; an explicit per-sync window overrides it.
        let window = window.window_for(&self.mailbox);
        let window = if window.is_bounded() {
            window
        } else {
            self.default_sync_window()
        };
        self.folder_stream(cursor, window, fetch_batch, chunk_size)
    }

    /// Fetches the bound folder's band of `widening` (`crate::band`): only the older mail the
    /// wider window adds, keeping the delta cursor, its position checkpointed at each fetch group.
    fn widen_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: &'a SyncState,
        widening: &'a Widening,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        self.band_stream(cursor, widening, fetch_batch, chunk_size)
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
        hand_over: &engine_provider::HandOver<'_>,
    ) -> ProviderResult<SubmissionReceipt> {
        self.submit(draft, hand_over).await
    }

    /// Files the Sent copy of an already-delivered message, for a host repairing a
    /// submission that came back `Unfiled` (`crate::filing`). Idempotent: it probes for the
    /// copy before placing one, on the first connection and on the retry alike.
    async fn file_sent_copy(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<ProviderKey> {
        self.refile(draft).await
    }

    /// Stores a draft in the account's `\\Drafts` folder (`APPEND`, carrying the keywords the
    /// folder keeps), removing the copy it supersedes.
    ///
    /// A thin borrow-and-call, like [`Provider::edit_mail`]: the ordering rule that decides
    /// whether a partial failure duplicates a draft or loses one lives in `crate::drafts`,
    /// so it stays stream-generic and unit-testable.
    async fn put_draft(
        &self,
        _account: &AccountId,
        draft: &Draft,
        replacing: Option<&ProviderKey>,
        metadata: &MessageMetadata,
    ) -> ProviderResult<DraftReceipt> {
        let mut connection = self.session().await?;
        let result = match self.store_on(&mut connection).await {
            Ok(store) => {
                crate::drafts::put_draft(&mut connection, &store, draft, replacing, metadata).await
            }
            Err(err) => Err(err),
        };
        connection.settle(result)
    }

    /// Removes a stored draft (`UID STORE \\Deleted` + `UID EXPUNGE`), reporting one that
    /// is already gone as done.
    async fn delete_draft(&self, _account: &AccountId, draft: &ProviderKey) -> ProviderResult<()> {
        let mut connection = self.session().await?;
        let result = match self.store_on(&mut connection).await {
            Ok(store) => crate::drafts::delete_draft(&mut connection, &store, draft).await,
            Err(err) => Err(err),
        };
        connection.settle(result)
    }

    /// Applies a [`MailEdit`] to the bound mailbox: mark-read/flag (`UID STORE`),
    /// move (`UID MOVE`), or permanent delete (`UID STORE \Deleted` + `UID EXPUNGE`).
    ///
    /// A thin borrow-and-call: the mutation logic (key parse, the SELECT + UIDVALIDITY
    /// guard, command dispatch) lives in the `mutate` module so it stays
    /// stream-generic and unit-testable. A stale UID (its mailbox's `UIDVALIDITY`
    /// changed) is a [`ProviderError::conflict`](engine_provider::ProviderError::conflict).
    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        let mut connection = self.session().await?;
        let result = crate::mutate::edit_mail(&mut connection, edit).await;
        connection.settle(result)
    }

    /// Fetches a message's raw RFC 5322 source (`UID FETCH BODY.PEEK[]`).
    ///
    /// The fetch logic (which connection, key parse, the EXAMINE + UIDVALIDITY guard,
    /// the body read, one retry on a lost connection) lives in the `fetch` module so it
    /// stays stream-generic and unit-testable. The message is addressed by its own key,
    /// so any of the account's folders can be read through any folder's provider; a
    /// stale UID (its mailbox's `UIDVALIDITY` changed) is a
    /// [`ProviderError::conflict`](engine_provider::ProviderError::conflict).
    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<engine_core::raw::RawMime> {
        crate::fetch::fetch_from_pool(&self.pool, message.id.key()).await
    }

    /// Fetches many messages' sources with one `UID FETCH <set> (BODY.PEEK[])` per mailbox,
    /// streaming each as it arrives (`crate::fetch_batch`). Any folder's provider can fetch
    /// any of the account's messages, as [`Provider::fetch_message_source`] can.
    fn fetch_message_sources<'a>(
        &'a self,
        _account: &'a AccountId,
        messages: &'a [Message],
    ) -> SourceStream<'a> {
        crate::fetch_batch::fetch_batch(&self.pool, messages)
    }

    /// Reports a message as junk / not junk / phishing.
    ///
    /// A thin borrow-and-call, like [`Provider::edit_mail`]: the keyword choice, the
    /// `PERMANENTFLAGS` check and the move live in `crate::report` so they stay
    /// stream-generic and unit-testable.
    async fn report_message(
        &self,
        _account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        let mut connection = self.session().await?;
        let result = crate::report::report_message(&mut connection, report).await;
        connection.settle(result)
    }

    /// The stores this credential can open besides its own: one per owner under each
    /// foreign namespace (`crate::folders`). Its own store is never among them — its
    /// prefix is the empty string, which is no handle — and the answer is the same
    /// whichever store this provider covers. Resolving an address reads this list through
    /// the trait's default.
    async fn list_shared_accounts(&self) -> ProviderResult<Vec<SharedAccount>> {
        if self.connection_info.capabilities.shared_accounts() != SharedAccounts::Enumerable {
            return Err(ProviderError::invalid_state(
                "provider does not support listing shared mailboxes",
            ));
        }
        let mut connection = self.session().await?;
        let found = match self.namespaces_on(&mut connection).await {
            Ok(namespaces) => crate::folders::discover(&mut connection, &namespaces).await,
            Err(err) => Err(err),
        };
        connection.settle(found)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> CalendarSync for ImapProvider<S> {}

/// Changes the folder tree of the store this provider covers (`CREATE`, `RENAME`, `DELETE`)
/// over a pooled session.
///
/// A thin borrow-and-call, like [`Provider::edit_mail`]: the path building, the fallbacks and
/// the subscription bookkeeping live in `crate::mailbox_write`.
#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> MailboxWrites for ImapProvider<S> {
    async fn edit_mailbox(
        &self,
        _account: &AccountId,
        edit: &MailboxEdit,
    ) -> ProviderResult<MailboxEditReceipt> {
        let capabilities = self.connection_info().capabilities;
        let mut connection = self.session().await?;
        let result = match self.store_on(&mut connection).await {
            Ok(store) => {
                crate::mailbox_write::edit_mailbox(&mut connection, &store, edit, &capabilities)
                    .await
            }
            Err(err) => Err(err),
        };
        connection.settle(result)
    }
}

impl<S> CalendarWrites for ImapProvider<S> where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static
{
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
