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
    EventWrite, EventWriteReceipt, MailEdit, MailEditReceipt, Provider, ProviderResult, ScopeSync,
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
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        (**self).delete_event(account, deletion).await
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
mod contacts_tests {
    use engine_core::{
        contact::{ContactField, ContactFieldSet, ContactSourceClass},
        ids::AddressBookId,
        membership::Memberships,
        sync::{JmapDataType, SyncUpdate},
    };

    use super::*;
    use crate::{Capabilities, ContactUnavailable, ScopeSync, WriteGuard};

    fn account() -> AccountId {
        AccountId::try_from("account").unwrap()
    }

    fn card() -> ContactCard {
        ContactCard::new(
            ContactId::try_from("contact").unwrap(),
            Memberships::of_one(AddressBookId::try_from("book").unwrap()),
        )
    }

    /// An adapter that overrides *every* contacts method, so a lost delegation shows.
    ///
    /// Each override answers something the trait default cannot: a success where the
    /// default errors, and a deliberately different scope where the default returns a
    /// fixed one.
    struct Supported;

    #[async_trait]
    impl Provider for Supported {
        fn connection_info(&self) -> ConnectionInfo {
            ConnectionInfo::new(Capabilities::none().with_contacts())
        }
    }

    #[async_trait]
    impl ContactsProvider for Supported {
        // Deliberately NOT the trait defaults' `AddressBook`/`ContactCard` data types: a
        // lost delegation would still return a plausible scope, so the assertion has to
        // be able to tell the override apart from the default.
        fn address_book_scope(&self, account: &AccountId) -> SyncScope {
            SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::Mailbox,
            }
        }

        fn contact_scope(&self, account: &AccountId) -> SyncScope {
            SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::Email,
            }
        }

        fn contact_destination(&self) -> Option<ContactDestination> {
            Some(ContactDestination {
                address_book: AddressBookId::try_from("book").unwrap(),
                source_class: ContactSourceClass::Personal,
                writable: true,
                write_guard: Some(WriteGuard::Absent),
                supported_fields: ContactFieldSet::from_fields([ContactField::Name]),
            })
        }

        async fn sync_address_books(
            &self,
            _account: &AccountId,
            _cursor: Option<&SyncState>,
        ) -> ProviderResult<ContactSourceSync<AddressBook>> {
            Ok(ContactSourceSync::Available {
                sync: ScopeSync::new(
                    SyncUpdate::delta(
                        vec![AddressBook::new(
                            AddressBookId::try_from("book").unwrap(),
                            "Personal",
                            ContactSourceClass::Personal,
                        )],
                        vec![],
                    ),
                    SyncState::new("books-1"),
                ),
                cursor_recovered: false,
            })
        }

        async fn sync_contacts(
            &self,
            _account: &AccountId,
            _cursor: Option<&SyncState>,
        ) -> ProviderResult<ContactSourceSync<ContactCard>> {
            // `Unavailable` rather than `Available`: it is the variant a *sibling* source
            // declining to sync produces, and flattening it to an error at the box
            // boundary would turn "this one book is unreadable" into "contacts are
            // broken". Delegation must preserve the variant, not just the `Ok`.
            Ok(ContactSourceSync::Unavailable(ContactUnavailable {
                reason: "missing permission".into(),
            }))
        }

        async fn fetch_contact(
            &self,
            _account: &AccountId,
            _contact: &ContactId,
        ) -> ProviderResult<ContactCard> {
            Ok(card())
        }

        async fn create_contact(
            &self,
            _account: &AccountId,
            _draft: &ContactDraft,
        ) -> ProviderResult<ContactWriteReceipt> {
            Ok(ContactWriteReceipt::new(
                ContactId::try_from("created").unwrap(),
            ))
        }

        async fn patch_contact(
            &self,
            _account: &AccountId,
            _base: &ContactCard,
            _patch: &ContactPatch,
        ) -> ProviderResult<ContactWriteReceipt> {
            Ok(ContactWriteReceipt::new(
                ContactId::try_from("patched").unwrap(),
            ))
        }

        async fn delete_contact(
            &self,
            _account: &AccountId,
            _base: &ContactCard,
        ) -> ProviderResult<()> {
            Ok(())
        }

        async fn fetch_contact_photo(
            &self,
            _account: &AccountId,
            _card: &ContactCard,
            _media: &ContactResource,
        ) -> ProviderResult<Option<ContactPhoto>> {
            Ok(Some(ContactPhoto::new(
                vec![7],
                Some("image/png".into()),
                "rev-1",
            )))
        }
    }

    #[tokio::test]
    async fn box_dyn_contacts_provider_delegates_every_method_to_the_inner_adapter() {
        // Asserted method by method because **every** `ContactsProvider` method has a
        // default body that returns an error: a forward this impl forgot would not fail
        // to compile, it would quietly answer "provider does not support contact sync"
        // for an adapter that supports it perfectly well. So each assertion below has to
        // distinguish the override's answer from the default's.
        let boxed: Box<dyn ContactsProvider> = Box::new(Supported);
        let account = account();
        let card = card();
        let draft = ContactDraft {
            address_book: AddressBookId::try_from("book").unwrap(),
            card: card.clone(),
        };
        let patch = ContactPatch::default();
        let media = ContactResource {
            uri: "https://example.test/photo".into(),
            ..ContactResource::default()
        };

        // The scopes: the override's data types, never the trait defaults'.
        assert!(matches!(
            boxed.address_book_scope(&account),
            SyncScope::JmapType {
                data_type: JmapDataType::Mailbox,
                ..
            }
        ));
        assert!(matches!(
            boxed.contact_scope(&account),
            SyncScope::JmapType {
                data_type: JmapDataType::Email,
                ..
            }
        ));
        // The default is `None`, so `Some` can only have come from the inner adapter.
        assert!(boxed.contact_destination().is_some());
        assert!(boxed.sync_address_books(&account, None).await.is_ok());
        // The declining-source variant survives the box rather than collapsing to an error.
        assert!(matches!(
            boxed.sync_contacts(&account, None).await.unwrap(),
            ContactSourceSync::Unavailable(ContactUnavailable { reason }) if reason == "missing permission"
        ));
        // Every remaining method's default is an error, so reaching a value proves the forward.
        assert!(boxed.fetch_contact(&account, &card.id).await.is_ok());
        assert_eq!(
            boxed
                .create_contact(&account, &draft)
                .await
                .unwrap()
                .contact
                .as_str(),
            "created"
        );
        assert_eq!(
            boxed
                .patch_contact(&account, &card, &patch)
                .await
                .unwrap()
                .contact
                .as_str(),
            "patched"
        );
        assert!(boxed.delete_contact(&account, &card).await.is_ok());
        assert_eq!(
            boxed
                .fetch_contact_photo(&account, &card, &media)
                .await
                .unwrap()
                .expect("the override answers with a photo")
                .as_bytes(),
            &[7]
        );
    }
}
