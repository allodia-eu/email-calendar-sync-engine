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
    ids::{AccountId, AddressBookId},
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncWindow},
};
use engine_provider::{
    Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, HttpVersion, PageToken, PassMode,
    Provider, ProviderResult, ScopeSync, SubmissionReceipt, SyncKind, split_page,
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
    /// The HTTP version the transport negotiated. Defaults to `None`: only the live
    /// [`JmapClient`] speaks HTTP, so a fake fed canned documents has no version to
    /// report.
    fn http_version(&self) -> Option<HttpVersion> {
        None
    }
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

    fn http_version(&self) -> Option<HttpVersion> {
        JmapClient::http_version(self)
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
    pub(crate) contact_address_book: AddressBookId,
}

impl core::fmt::Debug for JmapProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JmapProvider")
            .field("capabilities", &self.capabilities)
            .field("contact_address_book", &self.contact_address_book)
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
            contact_address_book: AddressBookId::try_from("default").expect("static id"),
        }
    }

    /// Binds host-facing contact writes to one discovered address book.
    #[must_use]
    pub fn with_contact_address_book(mut self, address_book: AddressBookId) -> Self {
        self.contact_address_book = address_book;
        self
    }

    pub(crate) fn executor(&self) -> &dyn Executor {
        self.executor.as_ref()
    }

    pub(crate) fn contact_account(&self) -> Result<String, JmapError> {
        Ok(self.executor.session().contact_account_id()?.to_owned())
    }

    pub(crate) async fn contact_call(
        &self,
        method: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, JmapError> {
        let mut request = Request::new([capability::CORE, capability::CONTACTS]);
        let call = request.invoke(method, arguments);
        let response = self.executor.execute(&request).await?;
        Ok(response.result(&call)?.clone())
    }

    pub(crate) async fn download_contact_blob(
        &self,
        blob: &str,
        media_type: Option<&str>,
    ) -> Result<Vec<u8>, JmapError> {
        let account = self.contact_account()?;
        let template = self
            .executor
            .session()
            .download_url()
            .ok_or_else(|| JmapError::session("server advertised no downloadUrl"))?;
        // `media_type` comes from the server's JSContact payload, not from a fixed
        // literal, so the substitution must be encoded: an unencoded `?`/`#`/`&`/`..`
        // in it would re-point or re-parameterize the download URL.
        let url = crate::blob::download_url(
            template,
            &account,
            blob,
            media_type.unwrap_or("application/octet-stream"),
            "contact-photo",
        );
        self.executor.download(&url).await
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
    /// The session's advertised capabilities plus the transport's negotiated HTTP
    /// version. The TLS version is always `None` — reqwest exposes only the peer
    /// certificate, never the negotiated protocol version (`docs/agent-guidance/tls.md`).
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.executor.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
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

    /// One `CalendarEvent/set` `create`. The **server** assigns the id, so the receipt is
    /// the only place the caller learns it (`crate::calendar_write`).
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &engine_provider::EventDraft,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        let account = self.calendar_account()?;
        Ok(crate::calendar_write::create_event(self.executor.as_ref(), &account, draft).await?)
    }

    /// One `CalendarEvent/set` `update`, whose PatchObject the **server** merges — so there
    /// is no document surgery on this transport, and no JSCalendar serializer to keep in
    /// step with the parser (`crate::calendar_write`).
    async fn patch_event(
        &self,
        _account: &AccountId,
        base: &Event,
        edit: &engine_provider::EventEdit,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        let account = self.calendar_account()?;
        Ok(
            crate::calendar_write::patch_event(self.executor.as_ref(), &account, base, edit)
                .await?,
        )
    }

    /// One `CalendarEvent/set` `destroy`. An already-gone event is a success, so a retried
    /// delete resolves cleanly (`crate::calendar_write`).
    async fn delete_event(
        &self,
        _account: &AccountId,
        deletion: &engine_provider::EventDeletion,
    ) -> ProviderResult<()> {
        let account = self.calendar_account()?;
        Ok(crate::calendar_write::delete_event(self.executor.as_ref(), &account, deletion).await?)
    }

    // `put_event` is deliberately **not** implemented: replacing a whole stored document is
    // the verb of a document-oriented transport, and JMAP has none — a JSCalendar object is
    // not a file the client owns the bytes of, and `/set` `update` is already a patch. It
    // stays at the trait's rejecting default even though this adapter advertises
    // `calendar_writes`, because that capability covers the neutral create/patch/delete
    // spine, not the escape hatch (`engine_provider::EventWrite`).

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
        Ok(crate::blob::message_source(self.executor.as_ref(), message).await?)
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

#[cfg(test)]
#[path = "calendar_write_support.rs"]
mod calendar_write_support;

#[cfg(test)]
#[path = "calendar_write_tests.rs"]
mod calendar_write_tests;

#[cfg(test)]
#[path = "calendar_patch_tests.rs"]
mod calendar_patch_tests;

#[cfg(test)]
#[path = "contact_tests.rs"]
mod contact_tests;
