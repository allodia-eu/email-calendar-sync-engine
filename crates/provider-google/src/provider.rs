//! The [`Provider`] implementation for Gmail: a Google client whose message sync is
//! **account-global**, with the label list synced at the account level.
//!
//! Unlike Graph/IMAP (per-folder message sync), Gmail's `historyId` is one account-wide
//! cursor (`jmap.md`-like), so a single [`GmailProvider`] syncs *all* of the account's
//! messages under [`SyncScope::GmailMessages`] — no per-label fan-out — while the label
//! list syncs under [`SyncScope::GmailLabelList`]. Labels are multi-membership on each
//! message, not the scope it was fetched under.

use std::{collections::BTreeSet, sync::Arc};

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
    SenderIdentity, SenderIdentityId, SourceStream, SubmissionReceipt,
};
use futures_util::StreamExt;
use provider_imap::GmailMail;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    fetch, imap_mail::ImapMail, mutate, named_labels::Labels, submit, transport::GoogleClient,
};

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
    /// The account's mail read over IMAP, when the host connected it
    /// ([`GmailProvider::with_imap`]).
    imap: Option<Arc<dyn ImapMail>>,
}

impl core::fmt::Debug for GmailProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GmailProvider")
            .field("capabilities", &self.capabilities)
            .field("imap", &self.imap.is_some())
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
            imap: None,
        }
    }

    /// Reads the account's mail over IMAP rather than the Gmail API: the message sync
    /// (`crate::imap_pass`) and every body. Labels, writes, drafts, sending, reports and
    /// identities stay on the API.
    ///
    /// The API bills a message's metadata 20 units and its body 60 against 6,000 per user per
    /// minute (`crate::quota`), which holds a sync to about 270 messages a minute; IMAP bills
    /// nothing per message and reads a thousand in about a second. Both write the same keys,
    /// threads, memberships and keywords, so an account moves between them without a resync
    /// of its rows (`crate::imap_labels`).
    #[must_use]
    pub fn with_imap<S>(self, mail: GmailMail<S>) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        self.with_imap_mail(Arc::new(mail))
    }

    pub(crate) fn with_imap_mail(mut self, imap: Arc<dyn ImapMail>) -> Self {
        self.imap = Some(imap);
        self
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
            ..match self.imap {
                // A body read over IMAP carries many sources in one `UID FETCH`; four at a time
                // leaves the account's fifth connection to the sync pass.
                Some(_) => ConnectionInfo::new(self.capabilities)
                    .with_concurrent_fetches(IMAP_CONCURRENT_READS)
                    .with_sources_per_request(IMAP_SOURCES_PER_REQUEST),
                None => ConnectionInfo::new(self.capabilities)
                    .with_concurrent_fetches(crate::fetch::MAX_CONCURRENT_GETS),
            }
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
        match &self.imap {
            Some(imap) => crate::imap_pass::stream(
                imap.as_ref(),
                &self.client,
                &self.labels,
                cursor,
                window.floor(),
                chunk_size,
            ),
            None => crate::pass::stream(
                &self.client,
                &self.labels,
                cursor,
                window.floor(),
                chunk_size,
            ),
        }
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        if let Some(imap) = &self.imap {
            let ids = [gmail_id(message.id.key())?];
            return match imap.sources(&ids).next().await {
                Some(Ok((_, source))) => Ok(source),
                Some(Err(err)) => Err(err),
                None => Err(gone(message.id.key())),
            };
        }
        // Gmail streams a message's raw RFC 5322 source from `messages.get?format=raw`
        // (base64url); the message's provider key is the Gmail message id.
        Ok(fetch::message_source(&self.client, message.id.key()).await?)
    }

    fn fetch_message_sources<'a>(
        &'a self,
        account: &'a AccountId,
        messages: &'a [Message],
    ) -> SourceStream<'a> {
        let Some(imap) = &self.imap else {
            return engine_provider::one_at_a_time(self, account, messages);
        };
        Box::pin(async_stream::stream! {
            let ids: Vec<Option<u64>> =
                messages.iter().map(|m| gmail_id(m.id.key()).ok()).collect();
            let wanted: Vec<u64> = ids.iter().flatten().copied().collect();
            let mut answered = vec![false; messages.len()];
            let mut sources = imap.sources(&wanted);
            while let Some(item) = sources.next().await {
                match item {
                    Ok((id, source)) => {
                        let Some(index) =
                            (0..messages.len()).find(|&at| !answered[at] && ids[at] == Some(id))
                        else {
                            continue;
                        };
                        answered[index] = true;
                        yield (index, Ok(source));
                    }
                    Err(err) => {
                        for (index, done) in answered.iter_mut().enumerate() {
                            if !*done {
                                *done = true;
                                let err =
                                    engine_provider::ProviderError::new(err.class(), err.detail());
                                yield (index, Err(err));
                            }
                        }
                    }
                }
            }
            for (index, message) in messages.iter().enumerate() {
                if !answered[index] {
                    yield (index, Err(gone(message.id.key())));
                }
            }
        })
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

/// Body reads in flight at once over IMAP.
const IMAP_CONCURRENT_READS: usize = 4;

/// Bodies one IMAP read carries (`provider_imap::GmailMail::sources` searches 50 ids a command).
const IMAP_SOURCES_PER_REQUEST: usize = 50;

/// The Gmail id a message key names: the key is that id in hex.
fn gmail_id(key: &ProviderKey) -> ProviderResult<u64> {
    u64::from_str_radix(key.as_str(), 16).map_err(|_| {
        engine_provider::ProviderError::invalid_state(format!(
            "not a Gmail message id: {}",
            key.as_str()
        ))
    })
}

/// A message no folder holds any more.
fn gone(key: &ProviderKey) -> engine_provider::ProviderError {
    engine_provider::ProviderError::conflict(format!(
        "message {} is no longer on the server: re-sync before reading it",
        key.as_str()
    ))
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "imap_bodies_tests.rs"]
mod imap_bodies_tests;
