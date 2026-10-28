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
    ids::{AccountId, ProviderKey},
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    CalendarWrites, Capabilities, ConnectionInfo, Draft, EmailStream, IdentityControls,
    KeywordName, MailEdit, MailEditReceipt, MailboxEdit, MailboxEditReceipt, MailboxWrites,
    Provider, ProviderResult, ReportControls, ReportEvidence, ReportVerdicts, ScopeSync,
    SenderIdentity, SenderIdentityId, SubmissionReceipt,
};

use crate::{fetch, mutate, named_labels::Labels, submit, transport::GoogleClient};

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
    /// The keywords kept as labels ([`GmailProvider::with_keyword_names`]).
    labels: Labels,
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
    ///
    /// Note what is **not** advertised: shared mailboxes. Gmail delegation is a real product
    /// feature, but not one a user credential reaches — the route that would serve it,
    /// `users/{userId}` for another user, needs a service account with domain-wide
    /// delegation. So the capability stays
    /// [`SharedMailboxes::Unsupported`](engine_provider::SharedMailboxes::Unsupported) and
    /// both discovery verbs keep their rejecting defaults, rather than offering an onboarding
    /// flow that could only fail (`google.md`).
    #[must_use]
    pub fn new(client: GoogleClient) -> Self {
        Self {
            client,
            capabilities: Capabilities::none()
                .with_mail()
                .with_message_source()
                .with_mail_writes()
                // Folders are labels, which the full `mail.google.com` scope may create,
                // rename and remove (`crate::labels_write`).
                .with_mailbox_writes()
                // Gmail keeps drafts as objects of their own, so a re-save replaces one
                // in place rather than creating and deleting (`crate::drafts`). Whether
                // *this* token holds the scope the collection needs is a scope question
                // no capability can answer, so a refusal surfaces as an error.
                .with_mail_drafts()
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
                .with_scheduling_submission()
                // Gmail's send-as settings are readable and writable by the account
                // holder. Whether *this* token may reach them is a scope question no
                // capability can answer, so a refusal surfaces as an error
                // (`crate::identity`).
                .with_sender_identities(IdentityControls::Writable),
            since: None,
            labels: Labels::default(),
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

    /// Keeps each of `names`' keywords as a Gmail label under one of its names.
    ///
    /// Gmail stores no free-form keyword, so a keyword a draft asks for on its filed copy
    /// ([`Draft::sent_copy_keywords`]) is kept only when it has a name here. A label under any
    /// of a keyword's names is then read as that keyword: it leaves the label list and every
    /// message's memberships, and the message carries the keyword instead. With at least one
    /// name, the adapter advertises [`sent_copy_keywords`](Capabilities::sent_copy_keywords).
    #[must_use]
    pub fn with_keyword_names(mut self, names: Vec<KeywordName>) -> Self {
        if !names.is_empty() {
            self.capabilities = self.capabilities.with_sent_copy_keywords();
        }
        self.labels = Labels::new(names);
        self
    }
}

#[async_trait]
impl Provider for GmailProvider {
    /// The fixed mail capabilities plus the versions the transport negotiated.
    ///
    /// Google has no session-discovery step, so [`GoogleClient::connect`] issues no
    /// request and both are `None` until this provider's first fetch
    /// (`docs/agent-guidance/tls.md`).
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            tls_version: self.client.tls_version(),
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
        let (mailboxes, named) = fetch::labels(&self.client, self.labels.names()).await?;
        self.labels.remember(named);
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
        // A sync-depth window bounds the *backfill* via `q: after:<floor>`. A delta
        // cannot carry one (`history.list` takes a `startHistoryId`, not a query), so a
        // relabelled old message is reported whatever its date; the engine drops it on
        // apply (`SyncWindow::admits`), which is where the delta's bound holds for every
        // adapter.
        crate::pass::stream(
            &self.client,
            &self.labels,
            cursor,
            window.floor(),
            chunk_size,
        )
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
    /// Stores a draft in `users.drafts`, replacing the one named by `replacing` in place.
    /// The key is Gmail's **draft** id, which survives the replacement (`crate::drafts`).
    async fn put_draft(
        &self,
        _account: &AccountId,
        draft: &engine_provider::Draft,
        replacing: Option<&ProviderKey>,
    ) -> ProviderResult<ProviderKey> {
        crate::drafts::put_draft(&self.client, draft, replacing).await
    }

    /// Removes a stored draft (`drafts.delete`), reporting one that is already gone as
    /// done.
    async fn delete_draft(&self, _account: &AccountId, draft: &ProviderKey) -> ProviderResult<()> {
        crate::drafts::delete_draft(&self.client, draft).await
    }

    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        let named = self.labels.resolved(&self.client).await?;
        mutate::edit(&self.client, edit, &named).await
    }

    /// Sends `draft` via `messages.send` in base64url MIME (`submit`), returning the sent
    /// copy's real Gmail id in the receipt. The outbox owns durability/idempotency.
    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
        hand_over: &engine_provider::HandOver<'_>,
    ) -> ProviderResult<SubmissionReceipt> {
        submit::send(&self.client, draft, &self.labels, hand_over).await
    }

    async fn sender_identities(&self, _account: &AccountId) -> ProviderResult<Vec<SenderIdentity>> {
        Ok(crate::identity::list(&self.client).await?)
    }

    async fn set_sender_name(
        &self,
        _account: &AccountId,
        identity: &SenderIdentityId,
        name: &str,
    ) -> ProviderResult<()> {
        Ok(crate::identity::set_name(&self.client, identity, name).await?)
    }

    async fn report_message(
        &self,
        _account: &AccountId,
        report: &engine_provider::MessageReport,
    ) -> ProviderResult<engine_provider::ReportReceipt> {
        crate::report::report_message(&self.client, report).await
    }
}

#[async_trait::async_trait]
impl MailboxWrites for GmailProvider {
    async fn edit_mailbox(
        &self,
        _account: &AccountId,
        edit: &MailboxEdit,
    ) -> ProviderResult<MailboxEditReceipt> {
        crate::labels_write::edit(&self.client, edit).await
    }
}

impl CalendarWrites for GmailProvider {}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
