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

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Mailbox, Message},
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    CalendarWrites, ConnectionInfo, Draft, EmailStream, MailEdit, MailEditReceipt, MessageReport,
    Provider, ProviderResult, ReportReceipt, ScopeSync, SubmissionReceipt,
};
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    filing::SmtpSender,
    mail::mailbox_from_list,
    pool::{ImapPool, PooledConnection},
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
    ) -> Self {
        Self {
            pool,
            mailbox,
            smtp,
            since,
            connection_info,
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
        let mut connection = self.session().await?;
        // The dialect decides how the names in these rows are encoded, and it is a property of
        // the session, so it is read from the same connection as the rows.
        let modified_utf7 = connection.names_are_modified_utf7();
        let listed = if connection
            .negotiated
            .has(crate::capability::Extension::ListStatus)
        {
            connection.list_with_unseen().await
        } else {
            match connection.list().await {
                Ok(rows) => crate::unseen::unseen_by_probing(&mut connection, &rows)
                    .await
                    .map(|unseen| (rows, unseen)),
                Err(err) => Err(err),
            }
        };
        let (rows, unseen) = connection.settle(listed)?;
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
        Box::pin(async_stream::stream! {
            let mut connection = match self.session().await {
                Ok(connection) => connection,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };
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
        let result = crate::drafts::put_draft(&mut connection, draft, replacing).await;
        connection.settle(result)
    }

    /// Removes a stored draft (`UID STORE \\Deleted` + `UID EXPUNGE`), reporting one that
    /// is already gone as done.
    async fn delete_draft(&self, _account: &AccountId, draft: &ProviderKey) -> ProviderResult<()> {
        let mut connection = self.session().await?;
        let result = crate::drafts::delete_draft(&mut connection, draft).await;
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
    /// A thin borrow-and-call: the fetch logic (key parse, the SELECT + UIDVALIDITY
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
        let mut connection = self.session().await?;
        let result = crate::fetch::fetch_message_source(&mut connection, message.id.key()).await;
        connection.settle(result)
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
