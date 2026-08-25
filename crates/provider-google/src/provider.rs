//! The [`Provider`] implementation for Gmail: a Google client whose message sync is
//! **account-global**, with the label list synced at the account level.
//!
//! Unlike Graph/IMAP (per-folder message sync), Gmail's `historyId` is one account-wide
//! cursor (`jmap.md`-like), so a single [`GmailProvider`] syncs *all* of the account's
//! messages under [`SyncScope::GmailMessages`] — no per-label fan-out — while the label
//! list syncs under [`SyncScope::GmailLabelList`]. Labels are multi-membership on each
//! message, not the scope it was fetched under.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    ids::{AccountId, ProviderKey},
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, MailEdit, MailEditReceipt,
    PageToken, PassMode, Provider, ProviderResult, ReportControls, ReportEvidence, ReportVerdicts,
    ScopeSync, SubmissionReceipt, SyncKind, split_page,
};

use crate::{fetch, mutate, submit, transport::GoogleClient};

/// The label list is re-discovered as a snapshot each pass (`GET /users/me/labels`), so
/// it carries no provider cursor of its own — like IMAP's folder list.
const LABEL_LIST_CURSOR: &str = "gmail-labels";

/// A Gmail read/sync provider. Its message scope is account-global; the label list syncs
/// under the account-level [`SyncScope::GmailLabelList`].
///
/// Construct one with [`GmailProvider::new`] from a connected [`GoogleClient`]. It
/// advertises mail read/sync and on-demand message-source fetch; submission and writes
/// are later slices.
pub struct GmailProvider {
    client: GoogleClient,
    capabilities: Capabilities,
    /// The default sync-depth cutoff the whole-scope [`Provider::sync_email`] drain
    /// windows its initial snapshot under (`None` syncs the whole account). Streaming
    /// callers pass a window per call.
    since: Option<CalendarDate>,
}

impl core::fmt::Debug for GmailProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GmailProvider")
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl GmailProvider {
    /// Binds a connected client for Gmail read/sync, on-demand source fetch, mail writes,
    /// and submission.
    #[must_use]
    pub fn new(client: GoogleClient) -> Self {
        Self {
            client,
            capabilities: Capabilities::none()
                .with_mail()
                .with_message_source()
                .with_mail_writes()
                // Junk and not-junk only: Gmail's system label set has no phishing member
                // and `messages.modify` 400s on anything outside it, so the verdict is
                // withheld rather than filed as junk. `Convention` because the filter is
                // documented to learn from the `SPAM` label and reports nothing back
                // (`crate::report`).
                .with_mail_report(ReportControls {
                    verdicts: ReportVerdicts::without_phishing(),
                    evidence: ReportEvidence::Convention,
                })
                // Both submission capabilities: this transport hands the server assembled RFC
                // 5322 bytes (`engine-rfc5322`), so it owns every `Content-Type` parameter —
                // including the `method=` that makes an iTIP object a scheduling message
                // rather than a calendar file (RFC 6047 §2.4). Contrast JMAP, which hands the
                // server a body structure and cannot.
                .with_submission()
                .with_scheduling_submission(),
            since: None,
        }
    }

    /// Sets the default sync-depth cutoff for the whole-scope [`Provider::sync_email`]
    /// drain: its initial snapshot is windowed to messages received on or after `since`
    /// (`q: after:<since>`). Streaming callers pass a window per call.
    #[must_use]
    pub fn with_since(mut self, since: CalendarDate) -> Self {
        self.since = Some(since);
        self
    }
}

#[async_trait]
impl Provider for GmailProvider {
    /// The fixed mail capabilities plus the transport's negotiated HTTP version.
    ///
    /// Google has no session-discovery step, so [`GoogleClient::connect`] issues no
    /// request and the HTTP version is `None` until this provider's first fetch. The TLS
    /// version is always `None`: reqwest exposes only the peer certificate
    /// (`docs/agent-guidance/tls.md`).
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            // The same ceiling the snapshot's own fan-out sits under
            // (`fetch::MAX_CONCURRENT_GETS`), reported so a caller draining a work list of
            // single fetches — warming bodies, most of all — overlaps them the same way
            // instead of paying one round trip per object.
            ..ConnectionInfo::new(self.capabilities)
                .with_concurrent_fetches(crate::fetch::MAX_CONCURRENT_GETS)
        }
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GmailLabelList {
            account: account.clone(),
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GmailMessages {
            account: account.clone(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let mailboxes = fetch::labels(&self.client).await?;
        // `labels.list` is a full snapshot every pass, so every label is present.
        let present: BTreeSet<ProviderKey> = mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(mailboxes, present),
            SyncState::new(LABEL_LIST_CURSOR),
        ))
    }

    /// The whole-scope drain windows under the cutoff fixed at construction
    /// ([`GmailProvider::with_since`]); [`Provider::stream_email`] takes its window per
    /// call.
    fn default_sync_window(&self) -> SyncWindow {
        self.since.map_or_else(SyncWindow::full, SyncWindow::since)
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        // Gmail pages `messages.list`/`history.list` by server-controlled `pageToken`, so
        // the fetch-batch knob has no lever here; each server page is drained whole.
        _fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        // A sync-depth window bounds the *snapshot* via `q: after:<floor>`. A delta
        // cannot carry one (`history.list` takes a `startHistoryId`, not a query), so a
        // relabelled old message is reported whatever its date; the engine drops it on
        // apply (`SyncWindow::admits`), which is where the delta's bound holds for every
        // adapter. The snapshot's bound is this query, and is not re-checked there.
        let floor = window.floor();
        Box::pin(async_stream::try_stream! {
            let mut page_token: Option<PageToken> = None;
            let mut mode: Option<PassMode> = None;
            // The delta cursor (a historyId); dropped to `None` to restart as a snapshot.
            let mut cursor = cursor;
            // The account historyId captured before a snapshot enumeration, carried as
            // the snapshot's persisted cursor.
            let mut snapshot_cursor: Option<SyncState> = None;
            let final_cursor = loop {
                let page = if let Some(delta_cursor) = cursor {
                    match fetch::delta_page(&self.client, delta_cursor, page_token.as_ref()).await {
                        Ok(page) => page,
                        // Gmail has aged the stored historyId out of its window (`404`):
                        // it cannot produce a delta from that cursor again. Drop it and
                        // restart the pass as a full snapshot. Only before the first page
                        // is committed (page_token none) and only on the first call —
                        // exactly like the Graph 410 restart: restarting mid-pass would
                        // leave later pages `Delta`, contributing nothing to `present`, and
                        // the reconcile would tombstone everything they returned.
                        Err(err)
                            if page_token.is_none()
                                && err.failure_class() == FailureClass::NeedsResync =>
                        {
                            cursor = None;
                            continue;
                        }
                        Err(err) => Err(err)?,
                    }
                } else {
                    // Snapshot: capture the account cursor once, before the first page, so
                    // messages arriving mid-snapshot are re-reported by the first delta.
                    if snapshot_cursor.is_none() {
                        snapshot_cursor = Some(fetch::current_history_id(&self.client).await?);
                    }
                    let history = snapshot_cursor.as_ref().expect("captured above");
                    fetch::snapshot_page(&self.client, page_token.as_ref(), floor, history).await?
                };

                // Decide the pass mode once, from the first page: a snapshot reconciles
                // (its present set tombstones absent rows); a delta is additive.
                let pass_mode = *mode.get_or_insert(match page.kind {
                    SyncKind::Snapshot => PassMode::Reconcile,
                    SyncKind::Delta => PassMode::Additive,
                });
                let is_last = page.next_page.is_none();
                let next_cursor = page.next_cursor.clone();
                for chunk in split_page(
                    pass_mode,
                    page.changed,
                    page.patched,
                    page.removed,
                    page.present,
                    None,
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
                    EmailChunk::additive(Vec::new(), Vec::new(), None, final_cursor)
                }
                PassMode::Reconcile => {
                    EmailChunk::reconcile_last(Vec::new(), Vec::new(), None, final_cursor)
                }
            };
        })
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        // Gmail streams a message's raw RFC 5322 source from `messages.get?format=raw`
        // (base64url); the message's provider key is the Gmail message id.
        Ok(fetch::message_source(&self.client, message.id.key()).await?)
    }

    /// Applies a [`MailEdit`] via `messages.modify`/`trash`/`delete` (`mutate`). The
    /// outbox owns durability/idempotency; this performs only the provider call.
    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        mutate::edit(&self.client, edit).await
    }

    /// Sends `draft` via `messages.send` in base64url MIME (`submit`), returning the sent
    /// copy's real Gmail id in the receipt. The outbox owns durability/idempotency.
    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        submit::send(&self.client, draft).await
    }

    async fn report_message(
        &self,
        _account: &AccountId,
        report: &engine_provider::MessageReport,
    ) -> ProviderResult<engine_provider::ReportReceipt> {
        crate::report::report_message(&self.client, report).await
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
