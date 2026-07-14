//! `Box<dyn Provider>` blanket implementation.
//!
//! Lets a host hold a provider adapter behind dynamic dispatch and still drive it
//! through the `engine-sync`/`engine-api` functions that are generic over
//! `P: Provider`.

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    ids::AccountId,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{SyncScope, SyncState, SyncWindow},
};

use crate::{
    ConnectionInfo, Draft, EmailStream, EventDeletion, EventDraft, EventEdit, EventWrite,
    EventWriteReceipt, MailEdit, MailEditReceipt, Provider, ProviderResult, ScopeSync,
    SubmissionReceipt,
};

/// A boxed provider is itself a [`Provider`], delegating every method to the box's
/// contents — including a `Box<dyn Provider>`, so a host can hold an adapter behind
/// dynamic dispatch.
///
/// The `engine-sync`/`engine-api` functions are generic over `P: Provider`, so a host
/// that picks a concrete adapter at runtime — e.g. a language binding choosing IMAP vs
/// JMAP from account config — needs this to drive them through a trait object. The
/// `?Sized` bound covers the trait-object case for *any* lifetime: a plain
/// `impl Provider for Box<dyn Provider>` is fixed to `'static` and is "not general
/// enough" once the boxed provider is driven from an async task. Kept here, not
/// special-cased in `engine-api` (`engine-api.md`). Every method delegates, so an inner
/// adapter's overrides (submission, calendar writes, a custom drain, …) are honored,
/// not the trait defaults.
#[async_trait]
impl<P: Provider + ?Sized> Provider for Box<P> {
    fn connection_info(&self) -> ConnectionInfo {
        (**self).connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        (**self).mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        (**self).email_scope(account)
    }

    async fn sync_mailboxes(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        (**self).sync_mailboxes(account, cursor).await
    }

    fn default_sync_window(&self) -> SyncWindow {
        (**self).default_sync_window()
    }

    fn stream_email<'a>(
        &'a self,
        account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        (**self).stream_email(account, cursor, window, fetch_batch, chunk_size)
    }

    async fn sync_email(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Message>> {
        (**self).sync_email(account, cursor).await
    }

    async fn submit_email(
        &self,
        account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        (**self).submit_email(account, draft).await
    }

    async fn edit_mail(
        &self,
        account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        (**self).edit_mail(account, edit).await
    }

    async fn fetch_message_source(
        &self,
        account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        (**self).fetch_message_source(account, message).await
    }

    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        (**self).calendar_scope(account)
    }

    fn event_scope(&self, account: &AccountId) -> SyncScope {
        (**self).event_scope(account)
    }

    async fn sync_calendars(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        (**self).sync_calendars(account, cursor).await
    }

    async fn sync_events(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        (**self).sync_events(account, cursor).await
    }

    async fn create_event(
        &self,
        account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        (**self).create_event(account, draft).await
    }

    async fn patch_event(
        &self,
        account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        (**self).patch_event(account, base, edit).await
    }

    async fn put_event(
        &self,
        account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        (**self).put_event(account, write).await
    }

    async fn delete_event(
        &self,
        account: &AccountId,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        (**self).delete_event(account, deletion).await
    }
}
