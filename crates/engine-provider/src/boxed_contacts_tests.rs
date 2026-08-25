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
