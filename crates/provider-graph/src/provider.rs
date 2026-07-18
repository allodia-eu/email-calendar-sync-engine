//! The [`Provider`] implementation: a Microsoft Graph client bound to one mail
//! folder for email, with the folder list synced at the account level.
//!
//! Graph mail `delta` is per-folder (`jmap.md`/this crate's docs), so — like
//! `provider-imap` — a [`GraphProvider`] is bound to a single folder: its
//! [`email_scope`](Provider::email_scope) names that folder
//! ([`SyncScope::GraphFolder`]) and [`stream_email`](Provider::stream_email)
//! streams its `messages/delta`. The folder list syncs under the per-account
//! [`SyncScope::GraphFolderList`]. The cross-folder fan-out is the orchestrator's
//! job.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, MailEdit, MailEditReceipt,
    PageToken, PassMode, Provider, ProviderResult, ScopeSync, SubmissionReceipt, SyncKind,
    split_page,
};

use crate::{fetch, transport::GraphClient};

/// The folder list is re-discovered as a snapshot each pass (`GET /me/mailFolders`),
/// so it carries no provider cursor of its own — like IMAP's folder list.
const FOLDER_LIST_CURSOR: &str = "graph-folders";

/// A Microsoft Graph read/sync provider bound to one mail folder for email.
///
/// Construct one with [`GraphProvider::new`] from a connected
/// [`GraphClient`](crate::GraphClient) and the folder to bind. It advertises mail
/// read/sync, mutating writes (mark-read/flag, move, delete), and submission; calendar
/// is a separate provider.
pub struct GraphProvider {
    client: GraphClient,
    folder: MailboxId,
    capabilities: Capabilities,
    /// The sync-depth cutoff the whole-scope drain ([`Provider::sync_email`]) syncs
    /// under via [`Provider::default_sync_window`]: when set, its initial message
    /// snapshot is windowed to messages received on or after this date (`None` syncs
    /// the whole folder). The streaming [`Provider::stream_email`] takes its window
    /// per call instead.
    since: Option<CalendarDate>,
}

impl core::fmt::Debug for GraphProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphProvider")
            .field("folder", &self.folder)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl GraphProvider {
    /// Binds a connected client to one mail folder for email sync.
    ///
    /// Submission is an **account-level** capability, not a folder one — `sendMail`
    /// posts to `/me/sendMail` and Graph files the Sent copy itself — so every bound
    /// `GraphProvider` advertises it. The cross-folder orchestrator submits through any
    /// one of an account's folder-bound providers.
    #[must_use]
    pub fn new(client: GraphClient, folder: MailboxId) -> Self {
        Self {
            client,
            folder,
            capabilities: Capabilities::none()
                .with_mail()
                .with_mail_writes()
                .with_submission(),
            since: None,
        }
    }

    /// Sets the default sync-depth cutoff for the whole-scope [`Provider::sync_email`]
    /// drain: its initial message snapshot is windowed to messages received on or after
    /// `since`. Later incremental syncs follow the server's deltaLink, which carries the
    /// window. Streaming callers pass a window per call to [`Provider::stream_email`].
    #[must_use]
    pub fn with_since(mut self, since: CalendarDate) -> Self {
        self.since = Some(since);
        self
    }
}

#[async_trait]
impl Provider for GraphProvider {
    /// The fixed mail capabilities plus the transport's negotiated HTTP version.
    ///
    /// Graph has no session-discovery step, so [`GraphClient::connect`] issues no
    /// request and the HTTP version is `None` until this provider's first fetch —
    /// unlike JMAP/CalDAV, which learn it while connecting. The TLS version is always
    /// `None`: reqwest exposes only the peer certificate, never the negotiated
    /// protocol version (`docs/agent-guidance/tls.md`).
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphFolderList {
            account: account.clone(),
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphFolder {
            account: account.clone(),
            folder: self.folder.clone(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let mailboxes = fetch::folders(&self.client).await?;
        // `GET /me/mailFolders` is a full snapshot every pass, so every folder is present.
        let present: BTreeSet<ProviderKey> = mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(mailboxes, present),
            SyncState::new(FOLDER_LIST_CURSOR),
        ))
    }

    /// The whole-scope [`Provider::sync_email`] drain windows under the cutoff fixed at
    /// construction ([`GraphProvider::with_since`]); [`Provider::stream_email`] takes its
    /// window per call.
    fn default_sync_window(&self) -> SyncWindow {
        self.since.map_or_else(SyncWindow::full, SyncWindow::since)
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        // Graph consumer `messages/delta` page size is server-controlled (`graph.md`),
        // so the fetch-batch knob has no lever here; each server page is drained whole.
        _fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        // A sync-depth window bounds a snapshot via a `receivedDateTime` `$filter`; a
        // delta ignores it (new arrivals are recent, and the deltaLink carries the
        // window).
        let floor = window.floor();
        Box::pin(async_stream::try_stream! {
            // Each Graph page is fetched whole over HTTP and re-chunked for incremental
            // commit; intermediate chunks hold the cursor and a final marker advances it
            // (a delta is not cheaply resumable mid-pass).
            let mut page_token: Option<PageToken> = None;
            let mut mode: Option<PassMode> = None;
            let mut total: Option<usize> = None;
            // Shadowed, so an aged-out deltaLink can be dropped and the pass restarted below.
            let mut cursor = cursor;
            let final_cursor = loop {
                let page = match fetch::messages_page(
                    &self.client,
                    &self.folder,
                    cursor,
                    page_token.as_ref(),
                    floor,
                )
                .await
                {
                    Ok(page) => page,
                    // Graph has expired the stored deltaLink (`410 SyncStateNotFound`): it cannot
                    // produce a delta from that cursor again, ever. Drop it and restart the pass
                    // as a full snapshot. Without this the folder is wedged for good — every pass
                    // replays the same dead cursor, upserts nothing, and no new mail is delivered
                    // again.
                    //
                    // It restarts the *pass*, not just this one call, and that distinction is
                    // load-bearing: `cursor` is what decides each page's `SyncKind`, and a
                    // reconciling pass tombstones against the `present` set that *every* page
                    // contributes. Re-fetching only this page without the cursor, while later
                    // pages still carried it, would leave those pages `Delta` — contributing
                    // nothing to `present` — and the reconcile would then tombstone every message
                    // they returned. Hence only before the first page is committed; after that
                    // the mode is already fixed and a restart would drop what was yielded.
                    Err(err)
                        if cursor.is_some()
                            && page_token.is_none()
                            && err.failure_class() == FailureClass::NeedsResync =>
                    {
                        cursor = None;
                        continue;
                    }
                    Err(err) => Err(err)?,
                };
                total = total.or(page.total);
                // Decide the pass mode once, from the first page: a snapshot (first sync)
                // reconciles — its present set tombstones absent rows; a delta is additive.
                let pass_mode = *mode.get_or_insert(match page.kind {
                    SyncKind::Snapshot => PassMode::Reconcile,
                    SyncKind::Delta => PassMode::Additive,
                });
                let is_last = page.next_page.is_none();
                let next_cursor = page.next_cursor.clone();
                for chunk in split_page(
                    pass_mode,
                    page.changed,
                    page.removed,
                    page.present,
                    total,
                    chunk_size,
                ) {
                    yield chunk;
                }
                if is_last {
                    break next_cursor;
                }
                page_token = page.next_page;
            };
            // The final marker carries the cursor (and, for reconcile, tombstones against
            // the accumulated present set).
            yield match mode.unwrap_or(PassMode::Additive) {
                PassMode::Additive => {
                    EmailChunk::additive(Vec::new(), Vec::new(), total, final_cursor)
                }
                PassMode::Reconcile => {
                    EmailChunk::reconcile_last(Vec::new(), Vec::new(), total, final_cursor)
                }
            };
        })
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        // Graph streams a message's full RFC 822 MIME from `/messages/{id}/$value`;
        // the message's provider key is that immutable id. One credential (the bound
        // client's token) backs the fetch, like every other call on this provider.
        Ok(fetch::message_source(&self.client, message.id.key()).await?)
    }

    /// Sends `draft` via `POST /me/sendMail` in MIME format (`submit`), so the caller's
    /// pre-generated `Message-ID` and threading survive to the wire. The outbox owns
    /// durability/idempotency; this performs only the provider call.
    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        crate::submit::send(&self.client, draft).await
    }

    /// Applies a [`MailEdit`] to an already-synced message: mark-read/flag (a `PATCH` of
    /// `isRead`/`flag`), move (`POST …/move`), or permanent delete (`POST …/permanentDelete`).
    ///
    /// The target's mailbox comes from its provider key, not this provider's bound folder,
    /// so one connected provider can edit a message in any of the account's folders — Graph
    /// addresses a message by its immutable id alone (`crate::mutate`).
    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        crate::mutate::edit_mail(&self.client, edit).await
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
