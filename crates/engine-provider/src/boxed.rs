//! `Box<dyn Provider>` and `Box<dyn ContactsProvider>` blanket implementations.
//!
//! Lets a host hold a provider adapter behind dynamic dispatch and still drive it
//! through the `engine-sync`/`engine-api` functions that are generic over
//! `P: Provider` / `P: ContactsProvider`.

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    contact::{AddressBook, ContactCard, ContactDraft, ContactPatch, ContactResource},
    ids::{AccountId, ContactId, ProviderKey},
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{SyncScope, SyncState, SyncWindow},
};

use crate::{
    ConnectionInfo, ContactDestination, ContactPhoto, ContactSourceSync, ContactWriteReceipt,
    ContactsProvider, Draft, EmailStream, EventDeletion, EventDraft, EventEdit, EventRsvp,
    EventWrite, EventWriteReceipt, MailEdit, MailEditReceipt, MessageReport, Provider,
    ProviderResult, ReportReceipt, ScopeSync, SubmissionReceipt,
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

    async fn file_sent_copy(
        &self,
        account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<ProviderKey> {
        (**self).file_sent_copy(account, draft).await
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

    async fn report_message(
        &self,
        account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        (**self).report_message(account, report).await
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

    async fn rsvp_event(
        &self,
        account: &AccountId,
        base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        (**self).rsvp_event(account, base, rsvp).await
    }

    async fn delete_event(
        &self,
        account: &AccountId,
        base: Option<&Event>,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        (**self).delete_event(account, base, deletion).await
    }
}

/// A boxed contacts adapter is itself a [`ContactsProvider`], for the same reason
/// its [`Provider`] counterpart above exists: `engine-sync`/`engine-api`'s contact
/// entry points are generic over a **sized** `P: ContactsProvider`, so a host that
/// resolves its adapter at runtime — CardDAV vs JMAP, decided by account config —
/// cannot reach them through a trait object without this.
///
/// Delegation matters more here than anywhere else in this file: **every method of
/// [`ContactsProvider`] has a default body that returns an error**, so a forwarding
/// impl that missed one would not fail to compile — it would silently answer
/// "provider does not support contact sync" for an adapter that supports it
/// perfectly well. Each method below therefore forwards, and the tests assert it.
#[async_trait]
impl<P: ContactsProvider + ?Sized> ContactsProvider for Box<P> {
    fn address_book_scope(&self, account: &AccountId) -> SyncScope {
        (**self).address_book_scope(account)
    }

    fn contact_scope(&self, account: &AccountId) -> SyncScope {
        (**self).contact_scope(account)
    }

    fn contact_destination(&self) -> Option<ContactDestination> {
        (**self).contact_destination()
    }

    async fn sync_address_books(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        (**self).sync_address_books(account, cursor).await
    }

    async fn sync_contacts(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        (**self).sync_contacts(account, cursor).await
    }

    async fn fetch_contact(
        &self,
        account: &AccountId,
        contact: &ContactId,
    ) -> ProviderResult<ContactCard> {
        (**self).fetch_contact(account, contact).await
    }

    async fn create_contact(
        &self,
        account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        (**self).create_contact(account, draft).await
    }

    async fn patch_contact(
        &self,
        account: &AccountId,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        (**self).patch_contact(account, base, patch).await
    }

    async fn delete_contact(&self, account: &AccountId, base: &ContactCard) -> ProviderResult<()> {
        (**self).delete_contact(account, base).await
    }

    async fn fetch_contact_photo(
        &self,
        account: &AccountId,
        card: &ContactCard,
        media: &ContactResource,
    ) -> ProviderResult<Option<ContactPhoto>> {
        (**self).fetch_contact_photo(account, card, media).await
    }
}

#[cfg(test)]
#[path = "boxed_contacts_tests.rs"]
mod contacts_tests;

#[cfg(test)]
#[path = "boxed_report_tests.rs"]
mod report_tests;
