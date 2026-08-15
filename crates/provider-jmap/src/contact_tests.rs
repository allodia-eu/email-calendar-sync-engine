use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactEmail, ContactField, ContactKind, ContactName,
        ContactPatch, ContactProperty, ContactResource, FieldPatch, NameComponentKind, PropertyId,
    },
    error::FailureClass,
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::RawJsContact,
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ContactSourceSync, ContactsProvider, Provider, WriteGuard};
use serde_json::json;

use super::provider_test_support::*;

#[tokio::test]
async fn contact_snapshot_preserves_jscontact_property_ids_and_raw_json() {
    let response = json!({
        "methodResponses": [[
            "ContactCard/get",
            {
                "accountId": "c",
                "state": "contacts-1",
                "list": [{
                    "id": "card-1",
                    "uid": "urn:uuid:card-1",
                    "kind": "individual",
                    "name": {
                        "full": "Zoë Example",
                        "components": [
                            { "kind": "given", "value": "Zoë" },
                            { "kind": "surname", "value": "Example" }
                        ]
                    },
                    "emails": {
                        "work": {
                            "address": "Zoe@xn--bcher-kva.example",
                            "contexts": { "work": true },
                            "pref": 1
                        }
                    },
                    "addressBookIds": { "book-1": true },
                    "x-example": { "untouched": true }
                }],
                "notFound": []
            },
            "0"
        ]]
    });
    let p = provider(vec![response]);
    let result = p.sync_contacts(&account(), None).await.unwrap();
    let engine_provider::ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected available contacts");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let card = &objects[0];
    assert_eq!(card.kind, ContactKind::Individual);
    let work = card
        .emails
        .iter()
        .find(|(id, _)| id.as_str() == "work")
        .map(|(_, email)| email)
        .unwrap();
    assert_eq!(work.value.address, "Zoe@xn--bcher-kva.example");
    assert!(work.contexts.contains("work"));
    let expected_raw = serde_json::to_string(&json!({
        "id": "card-1",
        "uid": "urn:uuid:card-1",
        "kind": "individual",
        "name": {
            "full": "Zoë Example",
            "components": [
                { "kind": "given", "value": "Zoë" },
                { "kind": "surname", "value": "Example" }
            ]
        },
        "emails": {
            "work": {
                "address": "Zoe@xn--bcher-kva.example",
                "contexts": { "work": true },
                "pref": 1
            }
        },
        "addressBookIds": { "book-1": true },
        "x-example": { "untouched": true }
    }))
    .unwrap();
    assert_eq!(
        card.raw_jscontact.as_ref().map(RawJsContact::as_str),
        Some(expected_raw.as_str())
    );
}

#[tokio::test]
async fn contact_create_sends_contactcard_set_with_explicit_membership() {
    let response = json!({
        "methodResponses": [[
            "ContactCard/set",
            { "created": { "new": { "id": "card-created" } } },
            "0"
        ]]
    });
    let (provider, executor) = recording(vec![response]);
    let book = AddressBookId::try_from("book-1").unwrap();
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
    let (using, method, arguments) = executor.sole_call();
    assert_eq!(
        using,
        ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"]
    );
    assert_eq!(method, "ContactCard/set");
    assert_eq!(arguments["create"]["new"]["addressBookIds"]["book-1"], true);
    assert_eq!(
        arguments["create"]["new"]["emails"]["work"]["address"],
        "ada@example.test"
    );
    assert!(arguments.get("ifInState").is_none());
}

#[test]
fn address_book_rights_defaults_and_raw_payload_are_preserved() {
    let book = crate::contact::address_book(&json!({
        "id": "book-1",
        "name": "Shared",
        "description": "Team contacts",
        "isDefault": true,
        "isSubscribed": false,
        "myRights": {
            "mayWrite": true,
            "mayShare": true,
            "mayDelete": false
        },
        "x-extra": true
    }))
    .unwrap();
    assert_eq!(book.id.as_str(), "book-1");
    assert_eq!(book.description.as_deref(), Some("Team contacts"));
    assert!(book.is_default);
    assert!(!book.is_subscribed);
    assert!(book.is_writable);
    assert!(book.rights.contains("mayWrite"));
    assert!(book.rights.contains("mayShare"));
    assert!(book.raw_provider_json.unwrap().as_str().contains("x-extra"));
    assert!(crate::contact::address_book(&json!({"name": "Missing id"})).is_err());
}

#[tokio::test]
async fn address_book_snapshot_and_contact_delta_use_contacts_capability() {
    let books = json!({
        "methodResponses": [[
            "AddressBook/get",
            {
                "accountId": "c",
                "state": "books-1",
                "list": [{"id": "book-1", "name": "Personal", "myRights": {"mayWrite": true}}]
            },
            "0"
        ]]
    });
    let (p, executor) = recording(vec![books]);
    let result = p.sync_address_books(&account(), None).await.unwrap();
    let ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected books");
    };
    assert!(matches!(
        sync.update,
        SyncUpdate::Snapshot { objects, present }
            if objects.len() == 1 && present.len() == 1
    ));
    let (using, method, arguments) = executor.sole_call();
    assert!(
        using
            .iter()
            .any(|value| value == "urn:ietf:params:jmap:contacts")
    );
    assert_eq!(method, "AddressBook/get");
    assert_eq!(arguments["accountId"], "c");

    let delta = json!({
        "methodResponses": [
            ["ContactCard/changes", {
                "oldState": "contacts-1",
                "newState": "contacts-2",
                "created": ["new"],
                "updated": ["updated"],
                "destroyed": ["gone"]
            }, "0"],
            ["ContactCard/get", {
                "state": "contacts-2",
                "list": [{"id": "new", "addressBookIds": {"book-1": true}}]
            }, "1"],
            ["ContactCard/get", {
                "state": "contacts-2",
                "list": [{"id": "updated", "addressBookIds": {"book-1": true}}]
            }, "2"]
        ]
    });
    let result = provider(vec![delta])
        .sync_contacts(&account(), Some(&SyncState::new("contacts-1")))
        .await
        .unwrap();
    let ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected contacts");
    };
    assert_eq!(sync.next_cursor.as_str(), "contacts-2");
    assert!(matches!(
        sync.update,
        SyncUpdate::Delta { changed, removed, .. }
            if changed.len() == 2 && removed.len() == 1
    ));
}

#[tokio::test]
async fn expired_contact_state_reports_recovery_to_a_snapshot() {
    let expired = json!({
        "methodResponses": [["error", {"type": "cannotCalculateChanges"}, "0"]]
    });
    let snapshot = json!({
        "methodResponses": [[
            "ContactCard/get",
            {
                "accountId": "c",
                "state": "fresh",
                "list": [{"id": "card-1", "addressBookIds": {"book-1": true}}]
            },
            "0"
        ]]
    });
    let result = provider(vec![expired, snapshot])
        .sync_contacts(&account(), Some(&SyncState::new("stale")))
        .await
        .unwrap();
    assert!(matches!(
        result,
        ContactSourceSync::Available {
            cursor_recovered: true,
            sync,
        } if matches!(sync.update, SyncUpdate::Snapshot { .. })
    ));
}

#[tokio::test]
async fn direct_fetch_patch_delete_and_photo_paths_are_exact() {
    let fetched = json!({
        "methodResponses": [[
            "ContactCard/get",
            {
                "list": [{
                    "id": "card-1",
                    "name": {"full": "Ada"},
                    "addressBookIds": {"book-1": true}
                }]
            },
            "0"
        ]]
    });
    let p = provider(vec![fetched]);
    let card = p
        .fetch_contact(&account(), &ContactId::try_from("card-1").unwrap())
        .await
        .unwrap();
    assert_eq!(card.display_name().as_deref(), Some("Ada"));

    let updated = json!({
        "methodResponses": [["ContactCard/set", {"updated": {"card-1": null}}, "0"]]
    });
    let (p, executor) = recording(vec![updated]);
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
    p.patch_contact(&account(), &card, &patch).await.unwrap();
    let (_, method, arguments) = executor.sole_call();
    assert_eq!(method, "ContactCard/set");
    assert_eq!(arguments["update"]["card-1"]["name"]["full"], "Updated");
    assert!(arguments.get("ifInState").is_none());

    let gone = json!({
        "methodResponses": [[
            "ContactCard/set",
            {"notDestroyed": {"card-1": {"type": "notFound"}}},
            "0"
        ]]
    });
    provider(vec![gone])
        .delete_contact(&account(), &card)
        .await
        .unwrap();

    let executor = FakeExecutor::new(vec![]).with_download_body(b"photo-bytes");
    let provider = super::JmapProvider::with_executor(Box::new(executor));
    let photo = provider
        .fetch_contact_photo(
            &account(),
            &card,
            &ContactResource {
                uri: "https://ignored.example/photo".into(),
                media_type: Some("image/jpeg".into()),
                fingerprint: Some("blob-1".into()),
                ..ContactResource::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(photo.as_bytes(), b"photo-bytes");
    assert_eq!(photo.fingerprint, "blob-1");
}

#[tokio::test]
async fn set_errors_missing_results_and_malformed_cards_surface() {
    let card = ContactCard::new(
        ContactId::try_from("card-1").unwrap(),
        Memberships::of_one(AddressBookId::try_from("book-1").unwrap()),
    );
    let mut patch = ContactPatch::default();
    patch.fields.insert(ContactField::Name, FieldPatch::Clear);
    let forbidden = json!({
        "methodResponses": [[
            "ContactCard/set",
            {"notUpdated": {"card-1": {"type": "forbidden"}}},
            "0"
        ]]
    });
    let error = provider(vec![forbidden])
        .patch_contact(&account(), &card, &patch)
        .await
        .unwrap_err();
    assert_eq!(error.class(), FailureClass::Permanent);

    let missing_create = json!({
        "methodResponses": [["ContactCard/set", {"created": {}}, "0"]]
    });
    let draft = ContactDraft {
        address_book: AddressBookId::try_from("book-1").unwrap(),
        card: card.clone(),
    };
    assert!(
        provider(vec![missing_create])
            .create_contact(&account(), &draft)
            .await
            .is_err()
    );
    let missing_fetch = json!({
        "methodResponses": [["ContactCard/get", {"list": []}, "0"]]
    });
    assert!(
        provider(vec![missing_fetch])
            .fetch_contact(&account(), &card.id)
            .await
            .is_err()
    );
    assert!(crate::contact::card(&json!({"id": "no-books"})).is_err());
    assert!(
        provider(vec![])
            .fetch_contact_photo(&account(), &card, &ContactResource::default())
            .await
            .is_err()
    );
}

#[test]
fn contact_capabilities_are_writable_without_a_per_card_guard() {
    let provider =
        provider(vec![]).with_contact_address_book(AddressBookId::try_from("book-1").unwrap());
    let capabilities = provider.connection_info().capabilities;
    assert!(capabilities.contacts());
    assert!(capabilities.contact_writes());
    assert!(capabilities.contact_groups());
    assert!(capabilities.contact_photos());
    assert_eq!(capabilities.contact_write_guard(), Some(WriteGuard::Absent));
    let destination = provider.contact_destination().unwrap();
    assert_eq!(destination.address_book.as_str(), "book-1");
    assert_eq!(destination.write_guard, Some(WriteGuard::Absent));
    assert!(
        !destination
            .supported_fields
            .contains(ContactField::TimeZone)
    );
}

/// JMAP has no well-known "default" address book, so an unbound provider must offer
/// no destination at all. Naming a fabricated book instead made a host's own
/// create-validation pass and left the server to reject `addressBookIds` on the wire —
/// the wrong layer to learn it in, and a "save to" picker offering a book that is not
/// there.
#[test]
fn an_unbound_provider_advertises_no_contact_destination() {
    assert!(provider(vec![]).contact_destination().is_none());
}

#[test]
fn card_kinds_members_media_and_structured_name_variants_are_normalized() {
    for (kind, expected) in [
        ("org", ContactKind::Organization),
        ("organization", ContactKind::Organization),
        ("group", ContactKind::Group),
        ("location", ContactKind::Location),
        ("device", ContactKind::Device),
        ("application", ContactKind::Application),
        ("x-kind", ContactKind::Other("x-kind".into())),
    ] {
        let card = crate::contact::card(&json!({
            "id": format!("card-{kind}"),
            "kind": kind,
            "addressBookIds": {"book-1": true}
        }))
        .unwrap();
        assert_eq!(card.kind, expected);
    }

    let card = crate::contact::card(&json!({
        "id": "group-1",
        "kind": "group",
        "addressBookIds": {"book-1": true, "ignored": false},
        "name": {"components": [
            {"kind": "title", "value": "Dr"},
            {"kind": "given2", "value": "Middle"},
            {"kind": "surname2", "value": "Second"},
            {"kind": "credential", "value": "PhD"},
            {"kind": "x-custom", "value": "Custom"}
        ]},
        // RFC 9553 §2.1.7: `members` is String[Boolean] — the KEY is the member's
        // uid and the value is `true`. There is no `Member` object, and no per-member
        // contexts/pref/label. A `false` entry is not a membership.
        "members": {
            "urn:uuid:person": true,
            "urn:uuid:not-a-member": false
        },
        "media": {"photo": {
            "uri": "https://contacts.example/photo",
            "kind": "photo",
            "mediaType": "image/jpeg",
            "title": "Portrait",
            "blobId": "blob-1"
        }},
        "isReadOnly": true
    }))
    .unwrap();
    assert_eq!(
        card.name
            .unwrap()
            .components
            .into_iter()
            .map(|component| component.kind)
            .collect::<Vec<_>>(),
        [
            NameComponentKind::Prefix,
            NameComponentKind::Middle,
            NameComponentKind::Surname2,
            NameComponentKind::Suffix,
            NameComponentKind::Other("x-custom".into())
        ]
    );
    // The member is keyed by its uid, and a `false` entry is not a member.
    assert_eq!(card.members.len(), 1);
    let member = card
        .members
        .get(&PropertyId::new("urn:uuid:person").unwrap())
        .unwrap();
    assert_eq!(member.value.uid, "urn:uuid:person");
    let photo = &card
        .media
        .get(&PropertyId::new("photo").unwrap())
        .unwrap()
        .value;
    assert_eq!(photo.kind.as_deref(), Some("photo"));
    assert_eq!(photo.media_type.as_deref(), Some("image/jpeg"));
    assert_eq!(photo.title.as_deref(), Some("Portrait"));
    assert_eq!(photo.fingerprint.as_deref(), Some("blob-1"));
    assert!(!card.is_writable);
}
