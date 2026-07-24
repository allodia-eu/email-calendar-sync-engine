use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactEmail, ContactKind, ContactPatch, ContactProperty,
        ContactResource, PropertyId,
    },
    ids::{AccountId, AddressBookId, ContactId},
    membership::Memberships,
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{ContactSourceSync, ContactsProvider, Provider, WriteGuard};
use serde_json::json;

use crate::{
    GraphClient, GraphContactProvider,
    test_support::{capturing_server, fake_client, fake_client_fallible, tls},
};

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

#[tokio::test]
async fn personal_contact_delta_preserves_change_key_and_raw_json() {
    let client = fake_client(vec![(
        "/contacts/delta",
        json!({
            "value": [{
                "id": "contact-1",
                "changeKey": "CQAAABY",
                "displayName": "Grace Hopper",
                "emailAddresses": [
                    { "name": "work", "address": "Grace@Example.COM" },
                    { "name": "home", "address": "g.hopper@example.net" }
                ]
            }],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/contacts/delta?$deltatoken=next"
        }),
    )]);
    let provider = GraphContactProvider::personal(client);
    assert_eq!(
        provider
            .connection_info()
            .capabilities
            .contact_write_guard(),
        Some(WriteGuard::Absent)
    );
    let result = provider.sync_contacts(&account(), None).await.unwrap();
    let engine_provider::ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    assert_eq!(objects[0].emails.len(), 2);
    assert_eq!(
        objects[0]
            .revisions
            .change_key
            .as_ref()
            .map(engine_core::version::ChangeKey::as_str),
        Some("CQAAABY")
    );
    assert!(objects[0].raw_provider_json.is_some());
}

#[tokio::test]
async fn create_uses_graph_contact_shape_without_a_conditional_guard() {
    let response = r#"{"id":"created-1","displayName":"Ada","emailAddresses":[]}"#;
    let (base, captured) = capturing_server("201 Created", response);
    let provider =
        GraphContactProvider::personal(GraphClient::with_base("token", base, tls()).unwrap());
    let book = AddressBookId::try_from("graph-personal-root").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("ignored").unwrap(),
        Memberships::of_one(book.clone()),
    );
    card.emails.insert(
        PropertyId::new("work").unwrap(),
        ContactProperty::new(ContactEmail::new("ada@example.test")),
    );
    provider
        .create_contact(
            &account(),
            &ContactDraft {
                address_book: book,
                card,
            },
        )
        .await
        .unwrap();
    let request = captured
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(request.starts_with("POST /me/contacts "), "{request}");
    assert!(!request.to_ascii_lowercase().contains("if-match:"));
    let body: serde_json::Value =
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body["emailAddresses"][0]["address"],
        json!("ada@example.test")
    );
}

#[test]
fn every_graph_contact_source_exposes_its_scope_capabilities_and_destination() {
    let personal = GraphContactProvider::personal(fake_client(vec![]));
    let folder = GraphContactProvider::personal_folder(
        fake_client(vec![]),
        AddressBookId::try_from("folder-1").unwrap(),
    );
    let organization = GraphContactProvider::organizational(fake_client(vec![]));
    let directory = GraphContactProvider::directory(fake_client(vec![]));
    assert!(personal.connection_info().capabilities.contact_writes());
    assert!(personal.connection_info().capabilities.contact_photos());
    let destination = personal.contact_destination().unwrap();
    assert!(
        !destination
            .supported_fields
            .contains(engine_core::contact::ContactField::Anniversaries)
    );
    assert!(
        !destination
            .supported_fields
            .contains(engine_core::contact::ContactField::Urls)
    );
    assert!(organization.contact_destination().is_none());
    assert!(directory.contact_destination().is_none());
    assert!(matches!(
        personal.address_book_scope(&account()),
        SyncScope::GraphContactFolderList { .. }
    ));
    assert!(matches!(
        folder.contact_scope(&account()),
        SyncScope::GraphContacts { address_book, .. } if address_book.as_str() == "folder-1"
    ));
    assert!(matches!(
        organization.contact_scope(&account()),
        SyncScope::GraphOrgContacts { .. }
    ));
    assert!(matches!(
        directory.contact_scope(&account()),
        SyncScope::GraphDirectoryUsers { .. }
    ));
    assert!(format!("{personal:?}").contains("Personal"));
}

#[tokio::test]
async fn contact_folder_discovery_recurses_and_drains_pages() {
    let provider = GraphContactProvider::personal(fake_client(vec![
        (
            "/contactFolders?$select",
            json!({
                "value": [{"id": "f1", "displayName": "One"}],
                "@odata.nextLink": "https://graph.test/next-folders"
            }),
        ),
        (
            "next-folders",
            json!({"value": [{"id": "f2", "displayName": "Two"}]}),
        ),
        (
            "/contactFolders/f1/childFolders",
            json!({"value": [{"id": "child", "displayName": "Child", "parentFolderId": "f1"}]}),
        ),
        ("/contactFolders/f2/childFolders", json!({"value": []})),
        ("/contactFolders/child/childFolders", json!({"value": []})),
    ]));
    let result = provider
        .sync_address_books(&account(), Some(&SyncState::new("ignored")))
        .await
        .unwrap();
    let ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, present } = sync.update else {
        panic!("expected snapshot");
    };
    assert_eq!(objects.len(), 4);
    assert_eq!(present.len(), 4);
    assert_eq!(sync.next_cursor.as_str(), "graph-contact-folders");
}

#[tokio::test]
async fn contact_delta_tombstone_cursor_recovery_and_permission_degradation() {
    let provider = GraphContactProvider::personal(fake_client(vec![(
        "stored-cursor",
        json!({
            "value": [
                {"id": "changed", "displayName": "Changed"},
                {"id": "removed", "@removed": {"reason": "deleted"}}
            ],
            "@odata.deltaLink": "next-cursor"
        }),
    )]));
    let result = provider
        .sync_contacts(&account(), Some(&SyncState::new("stored-cursor")))
        .await
        .unwrap();
    let ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected available");
    };
    assert!(matches!(
        sync.update,
        SyncUpdate::Delta { changed, removed }
            if changed.len() == 1 && removed.len() == 1
    ));

    let recovered = GraphContactProvider::personal(fake_client_fallible(vec![
        (
            "expired-cursor",
            Err((410, json!({"error":{"code":"SyncStateNotFound"}}))),
        ),
        (
            "/contacts/delta",
            Ok(json!({
                "value": [{"id": "recovered", "displayName": "Recovered"}],
                "@odata.deltaLink": "fresh"
            })),
        ),
    ]))
    .sync_contacts(&account(), Some(&SyncState::new("expired-cursor")))
    .await
    .unwrap();
    assert!(matches!(
        recovered,
        ContactSourceSync::Available {
            cursor_recovered: true,
            sync,
        } if matches!(sync.update, SyncUpdate::Snapshot { .. })
    ));

    let forbidden = vec![(
        "/contacts/delta",
        Err((403, json!({"error":{"code":"Authorization_RequestDenied"}}))),
    )];
    let optional = GraphContactProvider::organizational(fake_client_fallible(forbidden.clone()))
        .sync_contacts(&account(), None)
        .await
        .unwrap();
    assert!(matches!(optional, ContactSourceSync::Unavailable(_)));
    assert!(
        GraphContactProvider::personal(fake_client_fallible(forbidden))
            .sync_contacts(&account(), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn direct_fetch_photo_and_write_error_paths_are_source_targeted() {
    let book = AddressBookId::try_from("folder").unwrap();
    let provider = GraphContactProvider::personal_folder(
        fake_client_fallible(vec![
            (
                "/contactFolders/folder/contacts/c1/photo/$value",
                Ok(json!("photo-bytes")),
            ),
            (
                "/contactFolders/folder/contacts/c1",
                Ok(json!({"id": "c1", "displayName": "Ada", "changeKey": "v1"})),
            ),
            (
                "/contactFolders/folder/contacts",
                Ok(json!({"id": "created"})),
            ),
            (
                "/contactFolders/folder/contacts/gone",
                Err((404, json!({"error":{"code":"ErrorItemNotFound"}}))),
            ),
        ]),
        book.clone(),
    );
    let card = provider
        .fetch_contact(&account(), &ContactId::try_from("c1").unwrap())
        .await
        .unwrap();
    let photo = provider
        .fetch_contact_photo(&account(), &card, &ContactResource::default())
        .await
        .unwrap();
    assert_eq!(photo.as_bytes(), b"photo-bytes");
    assert_eq!(photo.fingerprint, "v1");

    let mut draft_card = card.clone();
    draft_card.address_books = Memberships::of_one(book.clone());
    let created = provider
        .create_contact(
            &account(),
            &ContactDraft {
                address_book: book,
                card: draft_card,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.contact.as_str(), "created");
    provider
        .patch_contact(&account(), &card, &ContactPatch::default())
        .await
        .unwrap();
    let mut gone = card.clone();
    gone.id = ContactId::try_from("gone").unwrap();
    provider.delete_contact(&account(), &gone).await.unwrap();

    let organization = GraphContactProvider::organizational(fake_client(vec![]));
    assert!(
        organization
            .patch_contact(&account(), &card, &ContactPatch::default())
            .await
            .is_err()
    );
    let mut group = card;
    group.kind = ContactKind::Group;
    assert!(
        provider
            .create_contact(
                &account(),
                &ContactDraft {
                    address_book: AddressBookId::try_from("folder").unwrap(),
                    card: group,
                },
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_contact_pages_fail_instead_of_advancing_a_cursor() {
    for page in [json!({"@odata.deltaLink": "next"}), json!({"value": []})] {
        assert!(
            GraphContactProvider::personal(fake_client(vec![("/contacts/delta", page)]))
                .sync_contacts(&account(), None)
                .await
                .is_err()
        );
    }
}
