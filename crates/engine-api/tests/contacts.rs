//! Contact facade sync, people paging, and outbox-write scenarios.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use engine_api::{
    AccountId, AddressBook, AddressBookId, Capabilities, ContactCard, ContactDestination,
    ContactDraft, ContactField, ContactFieldSet, ContactId, ContactKind, ContactPatch,
    ContactPhoto, ContactReconciled, ContactResource, ContactSourceClass, ContactsProvider, Engine,
    FieldPatch, PeopleQuery, Provider, ProviderKey, WriteGuard,
};
use engine_core::{
    contact::{
        ContactEmail, ContactMember, ContactName, ContactPhone, ContactProperty, Organization,
        PropertyId, Title,
    },
    membership::Memberships,
    sync::{SyncState, SyncUpdate},
    version::{ChangeKey, ETag, RevisionTokens},
};
use engine_provider::{
    ConnectionInfo, ContactSourceSync, ContactWriteReceipt, ProviderError, ProviderResult,
    ScopeSync,
};

#[derive(Default)]
struct FakeContacts {
    creates: AtomicUsize,
    patches: AtomicUsize,
    deletes: AtomicUsize,
    photos: AtomicUsize,
    fail_fetch: bool,
    read_only: bool,
    /// Answers every photo fetch with "this card has no image", the normal case for
    /// a correspondent outside the user's address books.
    no_photo: bool,
}

impl FakeContacts {
    fn card(id: &str, name: &str, email: &str) -> ContactCard {
        let mut card = ContactCard::new(
            ContactId::try_from(id).unwrap(),
            Memberships::of_one(AddressBookId::try_from("book").unwrap()),
        );
        card.name = Some(ContactName {
            full: Some(name.into()),
            ..ContactName::default()
        });
        card.uid = Some(id.into());
        card.emails.insert(
            PropertyId::new("email").unwrap(),
            ContactProperty::new(ContactEmail::new(email)),
        );
        card.is_writable = true;
        card
    }
}

#[async_trait]
impl Provider for FakeContacts {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(
            Capabilities::none()
                .with_contacts()
                .with_contact_writes(WriteGuard::Absent),
        )
    }
}

#[async_trait]
impl ContactsProvider for FakeContacts {
    fn contact_destination(&self) -> Option<ContactDestination> {
        Some(ContactDestination {
            address_book: AddressBookId::try_from("book").unwrap(),
            source_class: ContactSourceClass::Personal,
            writable: !self.read_only,
            write_guard: Some(WriteGuard::Absent),
            supported_fields: ContactFieldSet::from_fields([
                ContactField::Kind,
                ContactField::Name,
                ContactField::Emails,
            ]),
        })
    }

    async fn sync_address_books(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        let mut book = AddressBook::new(
            AddressBookId::try_from("book").unwrap(),
            "Contacts",
            ContactSourceClass::Personal,
        );
        book.is_writable = true;
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(
                    vec![book],
                    [ProviderKey::new("book").unwrap()].into_iter().collect(),
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
        let card = Self::card("c1", "Ada Lovelace", "Ada@Example.COM");
        let mut bob = Self::card("c3", "Bob Builder", "bob@example.test");
        bob.phones.insert(
            PropertyId::new("phone").unwrap(),
            ContactProperty::new(ContactPhone {
                number: "+31 20 555 0100".into(),
                ..ContactPhone::default()
            }),
        );
        bob.organizations.insert(
            PropertyId::new("organization").unwrap(),
            ContactProperty::new(Organization {
                name: "Construction Co".into(),
                ..Organization::default()
            }),
        );
        bob.titles.insert(
            PropertyId::new("title").unwrap(),
            ContactProperty::new(Title {
                name: "Foreman".into(),
                ..Title::default()
            }),
        );
        let mut group = ContactCard::new(
            ContactId::try_from("g1").unwrap(),
            Memberships::of_one(AddressBookId::try_from("book").unwrap()),
        );
        group.kind = ContactKind::Group;
        group.name = Some(ContactName {
            full: Some("Friends".into()),
            ..ContactName::default()
        });
        group.members.insert(
            PropertyId::new("member").unwrap(),
            ContactProperty::new(ContactMember::new("c1")),
        );
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(
                    vec![card, bob, group],
                    [
                        ProviderKey::new("c1").unwrap(),
                        ProviderKey::new("c3").unwrap(),
                        ProviderKey::new("g1").unwrap(),
                    ]
                    .into_iter()
                    .collect(),
                ),
                SyncState::new("contacts-1"),
            ),
            cursor_recovered: false,
        })
    }

    async fn fetch_contact(
        &self,
        _account: &AccountId,
        contact: &ContactId,
    ) -> ProviderResult<ContactCard> {
        if self.fail_fetch {
            return Err(ProviderError::retryable("contact fetch unavailable"));
        }
        Ok(Self::card(
            contact.as_str(),
            "Grace Hopper",
            "grace@example.test",
        ))
    }

    async fn create_contact(
        &self,
        _account: &AccountId,
        _draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok(ContactWriteReceipt::new(ContactId::try_from("c2").unwrap()))
    }

    async fn patch_contact(
        &self,
        _account: &AccountId,
        base: &ContactCard,
        _patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        self.patches.fetch_add(1, Ordering::SeqCst);
        Ok(ContactWriteReceipt::new(base.id.clone()))
    }

    async fn delete_contact(
        &self,
        _account: &AccountId,
        _base: &ContactCard,
    ) -> ProviderResult<()> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn fetch_contact_photo(
        &self,
        _account: &AccountId,
        _card: &ContactCard,
        media: &ContactResource,
    ) -> ProviderResult<Option<ContactPhoto>> {
        self.photos.fetch_add(1, Ordering::SeqCst);
        if self.no_photo {
            return Ok(None);
        }
        Ok(Some(ContactPhoto::new(
            vec![0xff, 0xd8, 0xff],
            Some("image/jpeg".into()),
            media
                .fingerprint
                .clone()
                .unwrap_or_else(|| media.uri.clone()),
        )))
    }
}

#[tokio::test]
async fn people_pages_are_generation_bound_and_contact_writes_refetch() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    engine.sync_contacts(&provider, &account).await.unwrap();

    let page = engine
        .people_page(&PeopleQuery {
            query: "ada".into(),
            limit: 10,
            ..PeopleQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.people.len(), 1);
    assert_eq!(page.people[0].display_name.as_deref(), Some("Ada Lovelace"));

    let draft = ContactDraft {
        address_book: AddressBookId::try_from("book").unwrap(),
        card: FakeContacts::card("ignored", "Grace Hopper", "grace@example.test"),
    };
    let write = engine
        .create_contact(&provider, &account, "create-grace", &draft)
        .await
        .unwrap();
    assert_eq!(write.write.contact.as_str(), "c2");
    assert!(matches!(write.reconciled, ContactReconciled::Applied(_)));
    assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
    assert!(
        engine
            .people_page(&PeopleQuery {
                query: "grace".into(),
                limit: 10,
                ..PeopleQuery::default()
            })
            .await
            .unwrap()
            .people
            .iter()
            .any(|person| person.display_name.as_deref() == Some("Grace Hopper"))
    );
}

#[tokio::test]
async fn unsupported_fields_are_rejected_before_provider_side_effects() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    engine.sync_contacts(&provider, &account).await.unwrap();
    let base = FakeContacts::card("c1", "Ada", "ada@example.test");
    let mut patch = ContactPatch {
        fields: BTreeMap::new(),
        ..ContactPatch::default()
    };
    patch
        .fields
        .insert(ContactField::Notes, FieldPatch::Set(serde_json::json!({})));
    let error = engine
        .patch_contact(&provider, &account, "unsupported", &base, &patch)
        .await
        .unwrap_err();
    assert!(matches!(error, engine_api::ApiError::InvalidInput(_)));
    assert_eq!(provider.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn people_can_be_filtered_by_synced_group_membership() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    engine.sync_contacts(&provider, &account).await.unwrap();
    let page = engine
        .people_page(&PeopleQuery {
            group: Some(ContactId::try_from("g1").unwrap()),
            limit: 10,
            ..PeopleQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.people.len(), 1);
    assert_eq!(page.people[0].display_name.as_deref(), Some("Ada Lovelace"));
}

#[tokio::test]
async fn people_paging_filters_cursor_validation_and_recipient_history_are_exposed() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    engine.sync_contacts(&provider, &account).await.unwrap();

    let first = engine
        .people_page(&PeopleQuery {
            limit: 1,
            ..PeopleQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        first.people[0].display_name.as_deref(),
        Some("Ada Lovelace")
    );
    let cursor = first.next_cursor.clone().unwrap();
    let second = engine
        .people_page(&PeopleQuery {
            cursor: Some(cursor.clone()),
            limit: 1,
            ..PeopleQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        second.people[0].display_name.as_deref(),
        Some("Bob Builder")
    );
    assert_eq!(
        engine
            .person(first.people[0].id)
            .await
            .unwrap()
            .unwrap()
            .display_name
            .as_deref(),
        Some("Ada Lovelace")
    );
    assert!(
        engine
            .person(engine_api::PersonId::new(999).unwrap())
            .await
            .unwrap()
            .is_none()
    );

    for query in ["+31", "construction", "foreman"] {
        assert_eq!(
            engine
                .people_page(&PeopleQuery {
                    query: query.into(),
                    account: Some(account.clone()),
                    address_book: Some(AddressBookId::try_from("book").unwrap()),
                    source_class: Some(ContactSourceClass::Personal),
                    writable: Some(true),
                    limit: 10,
                    ..PeopleQuery::default()
                })
                .await
                .unwrap()
                .people[0]
                .display_name
                .as_deref(),
            Some("Bob Builder")
        );
    }
    let groups = engine
        .people_page(&PeopleQuery {
            kind: Some(ContactKind::Group),
            limit: 10,
            ..PeopleQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(groups.people[0].display_name.as_deref(), Some("Friends"));

    for malformed in ["x", "00"] {
        let error = engine
            .people_page(&PeopleQuery {
                cursor: Some(malformed.into()),
                ..PeopleQuery::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(error, engine_api::ApiError::InvalidInput(_)));
    }
    let mismatch = engine
        .people_page(&PeopleQuery {
            query: "different".into(),
            cursor: Some(cursor.clone()),
            ..PeopleQuery::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(mismatch, engine_api::ApiError::InvalidInput(_)));
    engine.sync_contacts(&provider, &account).await.unwrap();
    let stale = engine
        .people_page(&PeopleQuery {
            cursor: Some(cursor),
            ..PeopleQuery::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(stale, engine_api::ApiError::InvalidInput(_)));

    let suggestions = engine.recipient_suggestions("bob", 500).await.unwrap();
    assert_eq!(suggestions.suggestions[0].display_name, "Bob Builder");
    assert!(suggestions.coverage.is_empty());
    assert!(engine.forget_recipient("not-an-email").await.is_err());
    assert_eq!(
        engine.forget_recipient("bob@example.test").await.unwrap(),
        0
    );
    assert_eq!(engine.clear_recipient_history(&account).await.unwrap(), 0);
    assert_eq!(engine.clear_all_recipient_history().await.unwrap(), 0);
}

#[path = "contact_cases/edges.rs"]
mod edges;

#[path = "contact_cases/photos.rs"]
mod photos;

#[path = "contact_cases/reads.rs"]
mod reads;
