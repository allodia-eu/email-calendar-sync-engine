//! The [`Provider`] implementation: wiring the JMAP session and account ids into
//! the generic mail/calendar read/sync ([`crate::fetch`]) and submission
//! ([`crate::submit`]).
//!
//! Each `sync_*` delegates to a shared container/member fetcher that picks
//! **snapshot** (first sync, or `cannotCalculateChanges` recovery) or **delta**
//! (`Foo/changes` → `Foo/get` over a result back-reference). Method execution goes
//! through the [`Executor`] seam so the orchestration is unit-tested offline
//! against captured Stalwart response documents; the live [`JmapClient`] is the
//! production executor.

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    ids::AccountId,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncWindow},
};
use engine_provider::{
    Capabilities, Draft, EmailChunk, EmailStream, PageToken, PassMode, Provider, ProviderResult,
    ScopeSync, SubmissionReceipt, SyncKind, split_page,
};
use serde_json::json;

use crate::{
    JmapClient, JmapConfig,
    calendar::{calendar_from_json, event_from_json},
    error::JmapError,
    fetch,
    fetch::MemberFetch,
    mail::{EMAIL_PROPERTIES, mailbox_from_json, message_from_json},
    request::{Request, Response, capability},
    session::Session,
};

/// Executes a batched JMAP request and exposes the session.
///
/// Implemented by the live [`JmapClient`] and, in tests, by a fake fed canned
/// response documents — so the sync orchestration is fully exercised offline.
#[async_trait]
pub(crate) trait Executor: Send + Sync {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError>;
    /// GETs raw bytes from a resolved blob-download URL (the raw message source).
    async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError>;
    /// POSTs raw `bytes` of `media_type` to a resolved blob-upload URL, returning the
    /// server-assigned `blobId` (RFC 8620 §6.1) — used to attach a draft's parts.
    async fn upload(&self, url: &str, media_type: &str, bytes: &[u8]) -> Result<String, JmapError>;
    fn session(&self) -> &Session;
}

#[async_trait]
impl Executor for JmapClient {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        JmapClient::execute(self, request).await
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        JmapClient::download(self, url).await
    }

    async fn upload(&self, url: &str, media_type: &str, bytes: &[u8]) -> Result<String, JmapError> {
        JmapClient::upload(self, url, media_type, bytes).await
    }

    fn session(&self) -> &Session {
        JmapClient::session(self)
    }
}

/// The JMAP provider adapter.
///
/// Construct one with [`JmapProvider::connect`]. It implements
/// [`engine_provider::Provider`] for the step-4 mail spine (mailboxes + email);
/// submission and calendar land in later slices.
pub struct JmapProvider {
    executor: Box<dyn Executor>,
    capabilities: Capabilities,
}

impl core::fmt::Debug for JmapProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JmapProvider")
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl JmapProvider {
    /// Connects to a JMAP server and discovers its session.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError`] on a connect/HTTP failure or a malformed session.
    pub async fn connect(config: JmapConfig) -> Result<Self, JmapError> {
        let client = JmapClient::connect(config).await?;
        Ok(Self::with_executor(Box::new(client)))
    }

    /// Wraps an executor, snapshotting its advertised capabilities.
    fn with_executor(executor: Box<dyn Executor>) -> Self {
        let capabilities = executor.session().capabilities();
        Self {
            executor,
            capabilities,
        }
    }

    /// The JMAP (server-side) mail account id for mail method arguments.
    fn mail_account(&self) -> Result<String, JmapError> {
        Ok(self.executor.session().mail_account_id()?.to_owned())
    }

    /// The JMAP (server-side) calendar account id for calendar method arguments.
    fn calendar_account(&self) -> Result<String, JmapError> {
        Ok(self.executor.session().calendar_account_id()?.to_owned())
    }
}

#[async_trait]
impl Provider for JmapProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Email,
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let account = self.mail_account()?;
        Ok(fetch::container_sync(
            self.executor.as_ref(),
            &account,
            &[capability::CORE, capability::MAIL],
            "Mailbox",
            cursor,
            mailbox_from_json,
            |mailbox| mailbox.id.key().clone(),
        )
        .await?)
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        // Newest-first, so a fresh sync surfaces recent mail before it finishes.
        let sort = json!([{ "property": "receivedAt", "isAscending": false }]);
        // A sync-depth window bounds a snapshot via `receivedAt` (RFC 8621 §4.4.1);
        // a delta ignores it (new arrivals are recent by definition).
        let filter = window
            .floor()
            .map(|date| json!({ "after": format!("{date}T00:00:00Z") }));
        Box::pin(async_stream::try_stream! {
            let account = self.mail_account()?;
            let fetch = MemberFetch {
                executor: self.executor.as_ref(),
                account: &account,
                using: &[capability::CORE, capability::MAIL],
                type_name: "Email",
                properties: Some(EMAIL_PROPERTIES),
            };
            // The JMAP round trip is atomic, so each page is fetched whole and
            // re-chunked for incremental commit; intermediate chunks hold the cursor
            // and a final marker advances it (JMAP is not cheaply resumable mid-pass).
            let mut page_token: Option<PageToken> = None;
            let mut mode: Option<PassMode> = None;
            let mut total: Option<usize> = None;
            let final_cursor = loop {
                let page = fetch::member_page(
                    &fetch,
                    sort.clone(),
                    cursor,
                    page_token.as_ref(),
                    fetch_batch,
                    filter.as_ref(),
                    message_from_json,
                )
                .await?;
                total = total.or(page.total);
                // Decide the pass mode once, from the first page. A JMAP page arrives
                // whole and is not cheaply resumable mid-pass, so a snapshot (first
                // sync or `cannotCalculateChanges`) reconciles — its present set
                // tombstones absent rows; a delta is additive.
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
            // The final marker carries the cursor (and, for reconcile, tombstones
            // against the accumulated present set).
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

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        let account = self.calendar_account()?;
        Ok(fetch::container_sync(
            self.executor.as_ref(),
            &account,
            &[capability::CORE, capability::CALENDARS],
            "Calendar",
            cursor,
            calendar_from_json,
            |calendar| calendar.id.key().clone(),
        )
        .await?)
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        let account = self.calendar_account()?;
        Ok(fetch::member_sync(
            self.executor.as_ref(),
            &account,
            &[capability::CORE, capability::CALENDARS],
            "CalendarEvent",
            None,
            cursor,
            event_from_json,
        )
        .await?)
    }

    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &engine_provider::MailEdit,
    ) -> ProviderResult<engine_provider::MailEditReceipt> {
        // All three edits (keyword patch / mailboxIds move / destroy) fold onto one
        // `Email/set`; the target's JMAP id is account-global, so the receipt key is
        // unchanged and the next sync reconciles membership (`crate::mutate`).
        let account = self.mail_account()?;
        Ok(crate::mutate::edit_mail(self.executor.as_ref(), &account, edit).await?)
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        // The message's raw RFC 5322 source is downloaded from the session's
        // `downloadUrl` blob template using the message's synced `blobId`; one
        // credential (the connected client) backs the fetch, like every other call.
        Ok(fetch::message_source(self.executor.as_ref(), message).await?)
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        let mail_account = self.executor.session().mail_account_id()?.to_owned();
        let submission_account = self.executor.session().submission_account_id()?.to_owned();
        Ok(crate::submit::send(
            self.executor.as_ref(),
            &mail_account,
            &submission_account,
            draft,
        )
        .await?)
    }
}

#[cfg(test)]
#[path = "provider_test_support.rs"]
mod provider_test_support;

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "provider_write_tests.rs"]
mod write_tests;
