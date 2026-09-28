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
    ids::{AccountId, MailboxId, ProviderKey, SharedMailboxId},
    mail::{Mailbox, Message},
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    CalendarWrites, ConnectionInfo, Draft, EmailStream, MailEdit, MailEditReceipt, MailboxEdit,
    MailboxEditReceipt, MailboxWrites, MessageReport, Provider, ProviderError, ProviderResult,
    ReportReceipt, ScopeSync, SharedMailbox, SharedMailboxes, SourceStream, SubmissionReceipt,
};
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    filing::SmtpSender,
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
    mailbox: MailboxId,
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
    /// The shared mail store this provider covers (`ImapAccount::with_shared_mailbox`), or
    /// `None` for the credential's own. `pub(crate)` for `crate::folders`, which turns it
    /// into the store every folder list and placement is scoped to.
    pub(crate) shared_mailbox: Option<SharedMailboxId>,
    /// The server's `NAMESPACE` answer, learned the first time something needs it
    /// (`crate::folders`) — never at connect, which most providers would pay for nothing —
    /// and shared by every provider of the account, so the login asks once.
    pub(crate) namespaces: Arc<OnceLock<Namespaces>>,
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
    /// A provider bound to `mailbox`, borrowing from `pool`. The account computes the rest once
    /// for all of its providers (`crate::account`).
    pub(crate) fn new(
        pool: Arc<ImapPool<S>>,
        mailbox: MailboxId,
        smtp: Option<Arc<SmtpSender>>,
        since: Option<time::Date>,
        connection_info: ConnectionInfo,
        shared_mailbox: Option<SharedMailboxId>,
        namespaces: Arc<OnceLock<Namespaces>>,
    ) -> Self {
        Self {
            pool,
            mailbox,
            smtp,
            since,
            connection_info,
            shared_mailbox,
            namespaces,
        }
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
        Box::pin(async_stream::stream! {
            let mut connection = match self.session().await {
                Ok(connection) => connection,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };
            // A provider scoped to a shared store syncs that store's folders only. Binding any
            // other folder to it would put that folder's mail in the shared mailbox's account
            // — refused, not synced.
            if self.shared_mailbox.is_some()
                && let Err(refusal) = self.check_in_store(&mut connection, &self.mailbox).await
            {
                connection.discard();
                yield Err(refusal);
                return;
            }
            // The stream holds its connection for as long as it lives, and hands it back
            // healthy only if every item was: a pass that failed mid-way has left the session
            // in a state no one should inherit. One abandoned mid-fetch parks normally — the
            // connection drains the leftover response before its next command.
            let mut failed = false;
            {
                let pass = crate::stream::stream_email(
                    &mut connection,
                    &self.mailbox,
                    cursor,
                    window,
                    fetch_batch,
                    chunk_size,
                );
                futures_util::pin_mut!(pass);
                while let Some(item) = pass.next().await {
                    failed = item.is_err();
                    yield item;
                }
            }
            if failed {
                connection.discard();
            }
        })
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
    /// copy before placing one, on the first connection and on the retry alike.
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
    /// A thin borrow-and-call, like [`Provider::edit_mail`]: the ordering rule that decides
    /// whether a partial failure duplicates a draft or loses one lives in `crate::drafts`,
    /// so it stays stream-generic and unit-testable.
    async fn put_draft(
        &self,
        _account: &AccountId,
        draft: &Draft,
        replacing: Option<&ProviderKey>,
    ) -> ProviderResult<ProviderKey> {
        let mut connection = self.session().await?;
        let result = match self.store_on(&mut connection).await {
            Ok(store) => crate::drafts::put_draft(&mut connection, &store, draft, replacing).await,
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
    async fn list_shared_mailboxes(&self) -> ProviderResult<Vec<SharedMailbox>> {
        if self.connection_info.capabilities.shared_mailboxes() != SharedMailboxes::Enumerable {
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
        let mut connection = self.session().await?;
        let result = match self.store_on(&mut connection).await {
            Ok(store) => crate::mailbox_write::edit_mailbox(&mut connection, &store, edit).await,
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
