use super::*;

struct OtherDestination;

#[async_trait]
impl Provider for OtherDestination {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_contacts())
    }
}

#[async_trait]
impl ContactsProvider for OtherDestination {
    fn contact_destination(&self) -> Option<ContactDestination> {
        Some(ContactDestination {
            address_book: AddressBookId::try_from("aaa").unwrap(),
            source_class: ContactSourceClass::Suggested,
            writable: false,
            write_guard: None,
            supported_fields: ContactFieldSet::new(),
        })
    }
}

#[tokio::test]
async fn destinations_and_source_targeted_patch_delete_are_exposed() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let other = OtherDestination;
    let account = AccountId::try_from("account-1").unwrap();
    engine.sync_contacts(&provider, &account).await.unwrap();

    assert_eq!(
        engine
            .contact_destination(&provider)
            .unwrap()
            .address_book
            .as_str(),
        "book"
    );
    let providers: [&dyn ContactsProvider; 3] = [&provider, &other, &provider];
    let destinations = engine.contact_destinations(&account, providers);
    assert_eq!(destinations.len(), 2);
    assert_eq!(destinations[0].address_book.as_str(), "aaa");
    assert_eq!(destinations[1].address_book.as_str(), "book");

    let base = FakeContacts::card("c1", "Ada", "ada@example.test");
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(serde_json::json!({"full": "Updated Ada"})),
    );
    let patched = engine
        .patch_contact(&provider, &account, "patch-ada", &base, &patch)
        .await
        .unwrap();
    assert!(matches!(patched.reconciled, ContactReconciled::Applied(_)));
    assert_eq!(provider.patches.load(Ordering::SeqCst), 1);

    let deleted = engine
        .delete_contact(&provider, &account, "delete-ada", &base)
        .await
        .unwrap();
    assert!(matches!(deleted.reconciled, ContactReconciled::Applied(_)));
    assert_eq!(provider.deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn create_validation_and_failed_reconciliation_are_explicit() {
    let engine = Engine::open_in_memory().unwrap();
    let account = AccountId::try_from("account-1").unwrap();
    let provider = FakeContacts::default();
    let wrong_book = ContactDraft {
        address_book: AddressBookId::try_from("wrong").unwrap(),
        card: FakeContacts::card("ignored", "Ada", "ada@example.test"),
    };
    assert!(
        engine
            .create_contact(&provider, &account, "wrong-book", &wrong_book)
            .await
            .is_err()
    );
    assert_eq!(provider.creates.load(Ordering::SeqCst), 0);

    let read_only = FakeContacts {
        read_only: true,
        ..FakeContacts::default()
    };
    assert!(
        engine
            .delete_contact(
                &read_only,
                &account,
                "read-only",
                &FakeContacts::card("c1", "Ada", "ada@example.test"),
            )
            .await
            .is_err()
    );

    let failing = FakeContacts {
        fail_fetch: true,
        ..FakeContacts::default()
    };
    engine.sync_contacts(&failing, &account).await.unwrap();
    let draft = ContactDraft {
        address_book: AddressBookId::try_from("book").unwrap(),
        card: FakeContacts::card("ignored", "Grace", "grace@example.test"),
    };
    let write = engine
        .create_contact(&failing, &account, "failed-refetch", &draft)
        .await
        .unwrap();
    assert!(matches!(write.reconciled, ContactReconciled::Failed(_)));
}

#[tokio::test]
async fn group_writes_are_rejected_before_reaching_the_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let account = AccountId::try_from("account-1").unwrap();
    let provider = FakeContacts::default();
    let mut group = FakeContacts::card("g1", "Friends", "group@example.test");
    group.kind = ContactKind::Group;
    group.members.insert(
        PropertyId::new("member").unwrap(),
        ContactProperty::new(ContactMember::new("c1")),
    );
    let draft = ContactDraft {
        address_book: AddressBookId::try_from("book").unwrap(),
        card: group.clone(),
    };

    assert!(
        engine
            .create_contact(&provider, &account, "group-create", &draft)
            .await
            .is_err()
    );
    assert!(
        engine
            .patch_contact(
                &provider,
                &account,
                "group-patch",
                &group,
                &ContactPatch::default(),
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .delete_contact(&provider, &account, "group-delete", &group)
            .await
            .is_err()
    );
    assert_eq!(provider.creates.load(Ordering::SeqCst), 0);
    assert_eq!(provider.patches.load(Ordering::SeqCst), 0);
    assert_eq!(provider.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn photo_cache_falls_back_through_etag_change_key_and_uri() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    let mut card = FakeContacts::card("c1", "Ada", "ada@example.test");
    let media = ContactResource {
        uri: "https://photos.test/ada".into(),
        ..ContactResource::default()
    };

    card.revisions = RevisionTokens::from_etag(ETag::new("\"v1\""));
    engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap();
    engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap();
    assert_eq!(provider.photos.load(Ordering::SeqCst), 1);

    card.revisions = RevisionTokens {
        change_key: Some(ChangeKey::new("change-2")),
        ..RevisionTokens::none()
    };
    engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap();
    card.revisions = RevisionTokens::none();
    engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap();
    assert_eq!(provider.photos.load(Ordering::SeqCst), 3);
}
