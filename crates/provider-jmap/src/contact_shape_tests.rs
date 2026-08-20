//! JMAP contact error surfacing, capability shape, and card normalization.

use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactField, ContactKind, ContactPatch, ContactResource,
        FieldPatch, NameComponentKind, PropertyId,
    },
    error::FailureClass,
    ids::{AddressBookId, ContactId},
    membership::Memberships,
};
use engine_provider::{ContactsProvider, Provider, WriteGuard};
use serde_json::json;

use super::provider_test_support::*;

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
