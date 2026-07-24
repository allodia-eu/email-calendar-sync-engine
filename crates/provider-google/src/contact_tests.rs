use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactField, ContactKind, ContactName, ContactPatch,
        ContactResource, ContactSourceClass, FieldPatch,
    },
    ids::{AccountId, AddressBookId, ContactId},
    membership::Memberships,
    raw::RawProviderJson,
    sync::{SyncState, SyncUpdate},
    version::{ETag, RevisionTokens},
};
use engine_provider::{ContactSourceSync, ContactsProvider, Provider, WriteGuard};
use serde_json::{Value, json};

use crate::{
    GoogleClient, GoogleContactProvider, GoogleContactSource,
    test_support::{capturing_server, fake_client, fake_client_fallible, tls},
};

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

#[tokio::test]
async fn update_contact_carries_the_source_etag_and_update_mask() {
    let (base_url, captured) = capturing_server("200 OK", r#"{"resourceName":"people/c1"}"#);
    let provider = GoogleContactProvider::connections(
        GoogleClient::with_base("token", base_url, tls()).unwrap(),
    );
    let mut base = ContactCard::new(
        ContactId::try_from("people/c1").unwrap(),
        Memberships::of_one(AddressBookId::try_from("google-connections").unwrap()),
    );
    base.revisions = RevisionTokens::from_etag(ETag::new("etag-source"));
    base.raw_provider_json = Some(RawProviderJson::new(
        r#"{"resourceName":"people/c1","etag":"etag-source","names":[]}"#,
    ));
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(
            serde_json::to_value(ContactName {
                full: Some("Updated".into()),
                ..ContactName::default()
            })
            .unwrap(),
        ),
    );
    provider
        .patch_contact(&account(), &base, &patch)
        .await
        .unwrap();
    let request = captured
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        request.starts_with("PATCH /v1/people/c1:updateContact?updatePersonFields=names&"),
        "{request}"
    );
    assert!(
        request.contains("personFields=names,nicknames,"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("if-match: etag-source"),
        "{request}"
    );
    let body: serde_json::Value =
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["etag"], "etag-source");
    assert_eq!(body["names"][0]["displayName"], "Updated");
}

#[tokio::test]
async fn owned_connections_snapshot_preserves_etag_and_raw_person() {
    let client = fake_client(vec![(
        "/v1/people/me/connections",
        json!({
            "connections": [{
                "resourceName": "people/c1",
                "etag": "%EgUBAi43PRo=",
                "names": [{ "displayName": "Ada Lovelace", "givenName": "Ada", "familyName": "Lovelace" }],
                "emailAddresses": [
                    { "value": "Ada@Example.COM", "type": "work", "metadata": { "primary": true } },
                    { "value": "ada@analytical.example", "type": "home" }
                ],
                "photos": [{ "url": "https://photos.test/ada", "default": false }]
            }],
            "nextSyncToken": "token-1"
        }),
    )]);
    let provider = GoogleContactProvider::connections(client);
    assert_eq!(
        provider
            .connection_info()
            .capabilities
            .contact_write_guard(),
        Some(WriteGuard::Enforced)
    );
    let result = provider.sync_contacts(&account(), None).await.unwrap();
    let engine_provider::ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected source");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let card = &objects[0];
    assert_eq!(card.source_class, ContactSourceClass::Personal);
    assert!(card.is_writable);
    assert_eq!(card.emails.len(), 2);
    assert_eq!(
        card.revisions
            .etag
            .as_ref()
            .map(engine_core::version::ETag::as_str),
        Some("%EgUBAi43PRo=")
    );
    assert!(card.raw_provider_json.is_some());
}

#[tokio::test]
async fn contact_groups_are_always_listed_as_a_snapshot_without_sync_token() {
    let (base_url, captured) = capturing_server(
        "200 OK",
        r#"{"contactGroups":[{"resourceName":"contactGroups/friends","name":"Friends"}]}"#,
    );
    let provider =
        GoogleContactProvider::groups(GoogleClient::with_base("token", base_url, tls()).unwrap());
    let result = provider
        .sync_contacts(&account(), Some(&SyncState::new("old-snapshot")))
        .await
        .unwrap();
    let engine_provider::ContactSourceSync::Available {
        sync,
        cursor_recovered,
    } = result
    else {
        panic!("expected source");
    };
    assert!(!cursor_recovered);
    assert!(matches!(sync.update, SyncUpdate::Snapshot { .. }));
    let request = captured
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let request_line = request.lines().next().unwrap();
    assert!(
        request_line.starts_with("GET /v1/contactGroups?"),
        "{request}"
    );
    assert!(!request_line.contains("syncToken"), "{request}");
}

#[test]
fn every_google_source_has_an_independent_scope_and_capability_contract() {
    let sources = [
        (
            GoogleContactProvider::connections(fake_client(vec![])),
            GoogleContactSource::Connections,
        ),
        (
            GoogleContactProvider::other_contacts(fake_client(vec![])),
            GoogleContactSource::OtherContacts,
        ),
        (
            GoogleContactProvider::directory(fake_client(vec![])),
            GoogleContactSource::Directory,
        ),
        (
            GoogleContactProvider::groups(fake_client(vec![])),
            GoogleContactSource::Groups,
        ),
    ];
    for (provider, source) in sources {
        assert!(matches!(
            provider.address_book_scope(&account()),
            engine_core::sync::SyncScope::GoogleContactSourceList { .. }
        ));
        let scope = provider.contact_scope(&account());
        assert_eq!(
            scope,
            match source {
                GoogleContactSource::Connections => {
                    engine_core::sync::SyncScope::GoogleContacts { account: account() }
                }
                GoogleContactSource::OtherContacts => {
                    engine_core::sync::SyncScope::GoogleOtherContacts { account: account() }
                }
                GoogleContactSource::Directory => {
                    engine_core::sync::SyncScope::GoogleDirectoryPeople { account: account() }
                }
                GoogleContactSource::Groups => {
                    engine_core::sync::SyncScope::GoogleContactGroups { account: account() }
                }
            }
        );
        assert!(provider.connection_info().capabilities.contacts());
        assert!(provider.connection_info().capabilities.contact_photos());
        assert_eq!(
            provider.contact_destination().is_some(),
            source == GoogleContactSource::Connections
        );
        assert!(format!("{provider:?}").contains("GoogleContactProvider"));
    }
}

#[tokio::test]
async fn source_discovery_and_paginated_snapshot_are_complete() {
    let provider = GoogleContactProvider::connections(fake_client(vec![
        (
            "pageToken=page-2",
            json!({
                "connections": [{"resourceName": "people/c2"}],
                "nextSyncToken": "sync-1"
            }),
        ),
        (
            "requestSyncToken=true",
            json!({
                "connections": [{"resourceName": "people/c1"}],
                "nextPageToken": "page-2"
            }),
        ),
    ]));
    let books = provider
        .sync_address_books(&account(), Some(&SyncState::new("ignored")))
        .await
        .unwrap();
    let ContactSourceSync::Available { sync, .. } = books else {
        panic!("expected sources");
    };
    assert!(matches!(
        sync.update,
        SyncUpdate::Snapshot { objects, present }
            if objects.len() == 4 && present.len() == 4
    ));

    let contacts = provider.sync_contacts(&account(), None).await.unwrap();
    let ContactSourceSync::Available { sync, .. } = contacts else {
        panic!("expected contacts");
    };
    assert_eq!(sync.next_cursor.as_str(), "sync-1");
    assert!(matches!(
        sync.update,
        SyncUpdate::Snapshot { objects, present }
            if objects.len() == 2 && present.len() == 2
    ));
}

#[tokio::test]
async fn deltas_tombstones_token_recovery_and_permissions_are_source_local() {
    let delta = GoogleContactProvider::other_contacts(fake_client(vec![(
        "syncToken=old",
        json!({
            "otherContacts": [
                {"resourceName": "people/changed"},
                {"resourceName": "people/removed", "metadata": {"deleted": true}}
            ],
            "nextSyncToken": "next"
        }),
    )]))
    .sync_contacts(&account(), Some(&SyncState::new("old")))
    .await
    .unwrap();
    assert!(matches!(
        delta,
        ContactSourceSync::Available {
            cursor_recovered: false,
            sync,
        } if matches!(
            &sync.update,
            SyncUpdate::Delta { changed, removed }
                if changed.len() == 1 && removed.len() == 1
        )
    ));

    let recovered = GoogleContactProvider::connections(fake_client_fallible(vec![
        (
            "syncToken=expired",
            Err((410, json!({"error": {"status": "EXPIRED_SYNC_TOKEN"}}))),
        ),
        (
            "requestSyncToken=true",
            Ok(json!({
                "connections": [{"resourceName": "people/recovered"}],
                "nextSyncToken": "fresh"
            })),
        ),
    ]))
    .sync_contacts(&account(), Some(&SyncState::new("expired")))
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
        "requestSyncToken=true",
        Err((403, json!({"error": {"status": "PERMISSION_DENIED"}}))),
    )];
    assert!(matches!(
        GoogleContactProvider::directory(fake_client_fallible(forbidden.clone()))
            .sync_contacts(&account(), None)
            .await
            .unwrap(),
        ContactSourceSync::Unavailable(_)
    ));
    assert!(
        GoogleContactProvider::connections(fake_client_fallible(forbidden))
            .sync_contacts(&account(), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn direct_fetch_writes_photos_and_read_only_rejections_are_targeted() {
    let provider = GoogleContactProvider::connections(fake_client_fallible(vec![
        (
            "/v1/people/c1?personFields",
            Ok(json!({
                "resourceName": "people/c1",
                "etag": "v1",
                "names": [{"displayName": "Ada"}]
            })),
        ),
        ("photos.test/c1", Ok(json!("photo-bytes"))),
        (
            "people:createContact",
            Ok(json!({"resourceName": "people/created"})),
        ),
        ("people/c1:updateContact", Ok(Value::Null)),
        (
            "people/c1:deleteContact",
            Err((404, json!({"error": {"status": "NOT_FOUND"}}))),
        ),
    ]));
    let card = provider
        .fetch_contact(&account(), &ContactId::try_from("people/c1").unwrap())
        .await
        .unwrap();
    let photo = provider
        .fetch_contact_photo(
            &account(),
            &card,
            &ContactResource {
                uri: "https://photos.test/c1".into(),
                media_type: Some("image/jpeg".into()),
                fingerprint: Some("photo-v1".into()),
                ..ContactResource::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(photo.as_bytes(), b"photo-bytes");
    assert_eq!(photo.media_type.as_deref(), Some("image/jpeg"));
    assert_eq!(photo.fingerprint, "photo-v1");

    let book = AddressBookId::try_from("google-connections").unwrap();
    let mut draft_card = card.clone();
    draft_card.address_books = Memberships::of_one(book.clone());
    assert_eq!(
        provider
            .create_contact(
                &account(),
                &ContactDraft {
                    address_book: book,
                    card: draft_card,
                },
            )
            .await
            .unwrap()
            .contact
            .as_str(),
        "people/created"
    );
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(
            serde_json::to_value(ContactName {
                full: Some("Updated".into()),
                ..ContactName::default()
            })
            .unwrap(),
        ),
    );
    provider
        .patch_contact(&account(), &card, &patch)
        .await
        .unwrap();
    provider.delete_contact(&account(), &card).await.unwrap();

    let read_only = GoogleContactProvider::directory(fake_client(vec![]));
    assert!(
        read_only
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
                    address_book: AddressBookId::try_from("google-connections").unwrap(),
                    card: group,
                },
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_pages_do_not_advance_contact_cursors() {
    for page in [json!({"nextSyncToken": "next"}), json!({"connections": []})] {
        assert!(
            GoogleContactProvider::connections(fake_client(vec![("requestSyncToken=true", page)]))
                .sync_contacts(&account(), None)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn directory_and_group_records_use_their_distinct_normalization() {
    let directory = GoogleContactProvider::directory(fake_client(vec![(
        "requestSyncToken=true",
        json!({
            "people": [{"resourceName": "people/directory"}],
            "nextSyncToken": "directory-next"
        }),
    )]))
    .sync_contacts(&account(), None)
    .await
    .unwrap();
    let ContactSourceSync::Available { sync, .. } = directory else {
        panic!("expected directory");
    };
    assert!(matches!(
        sync.update,
        SyncUpdate::Snapshot { objects, .. }
            if objects[0].source_class == ContactSourceClass::Directory
    ));

    let groups = GoogleContactProvider::groups(fake_client(vec![(
        "/v1/contactGroups",
        json!({
            "contactGroups": [{
                "resourceName": "contactGroups/friends",
                "name": "Friends"
            }]
        }),
    )]))
    .sync_contacts(&account(), Some(&SyncState::new("local-sentinel")))
    .await
    .unwrap();
    let ContactSourceSync::Available { sync, .. } = groups else {
        panic!("expected groups");
    };
    assert_eq!(sync.next_cursor.as_str(), "google-groups-snapshot");
    assert!(matches!(
        sync.update,
        SyncUpdate::Snapshot { objects, .. }
            if objects[0].kind == ContactKind::Group
    ));
}
