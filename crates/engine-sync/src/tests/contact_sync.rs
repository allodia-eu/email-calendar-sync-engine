//! Contact container/card ordering, availability, and people rebuild.

use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactEmail, ContactProperty, ContactSourceClass, PropertyId,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactUnavailable, ContactsProvider};
use engine_store::SyncApplied;

use super::*;

struct FakeContacts {
    unavailable: bool,
}

impl Provider for FakeContacts {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_contacts())
    }
}

#[async_trait::async_trait]
impl ContactsProvider for FakeContacts {
    async fn sync_address_books(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        let mut book = AddressBook::new(
            AddressBookId::try_from("personal").unwrap(),
            "Personal",
            ContactSourceClass::Personal,
        );
        book.is_writable = true;
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(vec![book], [ProviderKey::new("personal").unwrap()].into()),
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
        if self.unavailable {
            return Ok(ContactSourceSync::Unavailable(ContactUnavailable {
                reason: "directory permission missing".into(),
            }));
        }
        let cards = ["one", "two"].map(|id| {
            let mut card = ContactCard::new(
                ContactId::try_from(id).unwrap(),
                Memberships::of_one(AddressBookId::try_from("personal").unwrap()),
            );
            card.source_class = ContactSourceClass::Personal;
            card.is_writable = true;
            card.emails.insert(
                PropertyId::new("email").unwrap(),
                ContactProperty::new(ContactEmail::new("shared@example.test")),
            );
            card
        });
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(
                    cards.to_vec(),
                    cards.iter().map(|card| card.id.key().clone()).collect(),
                ),
                SyncState::new("cards-1"),
            ),
            cursor_recovered: true,
        })
    }
}

#[tokio::test]
async fn combined_contact_sync_rebuilds_one_unified_person() {
    let provider = FakeContacts { unavailable: false };
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let report = crate::sync_contacts(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(report.address_books.applied.upserted, 1);
    assert_eq!(report.cards.applied.upserted, 2);
    assert!(report.cards.cursor_recovered);
    assert_eq!(report.people.generation, 1);
    assert_eq!(report.people.people, 1);
    assert_eq!(store.people_snapshot().await.unwrap().people.len(), 1);
    assert_eq!(
        store
            .contact_source_availability(account())
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn optional_source_unavailability_is_reported_and_persisted() {
    let provider = FakeContacts { unavailable: true };
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let report = crate::sync_contacts(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(
        report.cards.unavailable.as_deref(),
        Some("directory permission missing")
    );
    assert_eq!(report.cards.applied, SyncApplied::default());
    assert!(
        store
            .contact_source_availability(account())
            .await
            .unwrap()
            .iter()
            .any(|(_, availability)| matches!(
                availability,
                engine_store::ContactSourceAvailability::Unavailable { .. }
            ))
    );
}
