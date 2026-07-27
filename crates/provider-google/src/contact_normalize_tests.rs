use engine_core::contact::{ContactKind, ContactSourceClass, NameComponentKind};
use serde_json::json;

use super::*;

fn book() -> AddressBookId {
    AddressBookId::try_from("google-connections").unwrap()
}

#[test]
fn comprehensive_person_normalizes_losslessly() {
    let card = person(
        &json!({
            "resourceName": "people/c1",
            "etag": "etag-1",
            "names": [{
                "displayName": "Dr Ada M Lovelace PhD",
                "honorificPrefix": "Dr",
                "givenName": "Ada",
                "middleName": "M",
                "familyName": "Lovelace",
                "honorificSuffix": "PhD"
            }],
            "nicknames": [{"value": "Enchantress"}],
            "emailAddresses": [{
                "value": "Ada@Example.COM",
                "type": "home",
                "metadata": {"primary": true, "source": {"id": "email-source"}}
            }],
            "phoneNumbers": [{"value": "+44 123", "type": "mobile"}],
            "addresses": [{
                "formattedValue": "1 Main St, London",
                "streetAddress": "1 Main St",
                "city": "London",
                "region": "London",
                "postalCode": "N1",
                "country": "United Kingdom",
                "countryCode": "GB"
            }],
            "organizations": [{
                "name": "Analytical Engines",
                "department": "Research",
                "title": "Programmer"
            }, {"name": "Second"}],
            "birthdays": [
                {"date": {"year": 1815, "month": 12, "day": 10}},
                {"metadata": {"primary": false}}
            ],
            "biographies": [{"value": "First programmer"}],
            "urls": [{"value": "https://ada.example.test", "type": "work"}],
            "relations": [{"person": "Charles Babbage", "type": "manager"}],
            "userDefined": [
                {"key": "category", "value": "mathematician"},
                {"key": "custom", "value": "kept"}
            ],
            "photos": [
                {"url": "https://photos.test/default", "default": true},
                {"url": "https://photos.test/custom", "default": false}
            ],
            "unknown": {"preserved": true}
        }),
        book(),
        ContactSourceClass::Personal,
        true,
    )
    .unwrap();
    assert_eq!(card.uid.as_deref(), Some("people/c1"));
    assert_eq!(card.name.as_ref().unwrap().components.len(), 5);
    assert_eq!(
        card.name.as_ref().unwrap().components[0].kind,
        NameComponentKind::Prefix
    );
    assert_eq!(card.nicknames.len(), 1);
    assert_eq!(card.emails.len(), 1);
    let email = card.emails.values().next().unwrap();
    assert!(email.contexts.contains("private"));
    assert_eq!(email.preference, Some(1));
    assert_eq!(card.phones.len(), 1);
    assert_eq!(card.addresses.len(), 1);
    assert_eq!(card.organizations.len(), 2);
    assert_eq!(card.titles.len(), 1);
    assert_eq!(card.anniversaries.len(), 1);
    assert_eq!(card.notes.len(), 1);
    assert_eq!(card.urls.len(), 1);
    assert_eq!(card.relations.len(), 1);
    assert!(card.keywords.contains("mathematician"));
    assert!(card.keywords.contains("custom:kept"));
    assert_eq!(card.media.len(), 1);
    assert_eq!(card.revisions.etag.unwrap().as_str(), "etag-1");
    assert!(card.raw_provider_json.unwrap().as_str().contains("unknown"));
}

#[test]
fn source_books_groups_deletions_and_malformed_records_are_explicit() {
    let books = source_books();
    assert_eq!(books.len(), 4);
    assert!(books[0].is_writable);
    assert_eq!(books[1].source_class, ContactSourceClass::Suggested);
    assert_eq!(books[2].source_class, ContactSourceClass::Directory);

    let group = group_card(
        &json!({
            "resourceName": "contactGroups/friends",
            "name": "Friends",
            "unknown": true
        }),
        AddressBookId::try_from("google-contact-groups").unwrap(),
    )
    .unwrap();
    assert_eq!(group.kind, ContactKind::Group);
    assert_eq!(group.display_name().as_deref(), Some("Friends"));
    assert!(
        group
            .raw_provider_json
            .unwrap()
            .as_str()
            .contains("unknown")
    );

    assert!(deleted(&json!({"metadata": {"deleted": true}})));
    assert!(!deleted(&json!({"metadata": {"deleted": false}})));
    assert!(person(&json!({}), book(), ContactSourceClass::Personal, true).is_err());
    assert!(
        group_card(
            &json!({"name": "Missing id"}),
            AddressBookId::try_from("groups").unwrap()
        )
        .is_err()
    );
}
