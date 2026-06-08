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
use engine_core::calendar::{Calendar, Event};
use engine_core::ids::AccountId;
use engine_core::mail::{Mailbox, Message};
use engine_core::sync::{JmapDataType, SyncScope, SyncState};
use engine_provider::{
    Capabilities, Draft, PageToken, Provider, ProviderResult, ScopeSync, SubmissionReceipt,
    SyncPage,
};
use serde_json::json;

use crate::calendar::{calendar_from_json, event_from_json};
use crate::error::JmapError;
use crate::fetch;
use crate::fetch::MemberFetch;
use crate::mail::{EMAIL_PROPERTIES, mailbox_from_json, message_from_json};
use crate::request::{Request, Response, capability};
use crate::session::Session;
use crate::{JmapClient, JmapConfig};

/// Executes a batched JMAP request and exposes the session.
///
/// Implemented by the live [`JmapClient`] and, in tests, by a fake fed canned
/// response documents — so the sync orchestration is fully exercised offline.
#[async_trait]
pub(crate) trait Executor: Send + Sync {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError>;
    fn session(&self) -> &Session;
}

#[async_trait]
impl Executor for JmapClient {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        JmapClient::execute(self, request).await
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

    async fn sync_email_page(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        let account = self.mail_account()?;
        let fetch = MemberFetch {
            executor: self.executor.as_ref(),
            account: &account,
            using: &[capability::CORE, capability::MAIL],
            type_name: "Email",
            properties: Some(EMAIL_PROPERTIES),
        };
        // Newest-first, so a fresh sync surfaces recent mail before it finishes.
        let sort = json!([{ "property": "receivedAt", "isAscending": false }]);
        Ok(fetch::member_page(&fetch, sort, cursor, page, limit, message_from_json).await?)
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
#[path = "provider_tests.rs"]
mod tests;
