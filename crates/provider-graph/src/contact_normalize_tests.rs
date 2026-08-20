//! Graph contact/folder normalization tests.

use serde_json::json;

use super::*;

#[test]
fn folder_and_comprehensive_contact_normalize_losslessly() {
    let folder = folder(&json!({
        "id": "folder-1",
        "displayName": "Friends",
        "parentFolderId": "root",
        "extension": {"kept": true}
    }))
    .unwrap();
    assert_eq!(folder.id.as_str(), "folder-1");
    assert_eq!(folder.owner.as_deref(), Some("root"));
    assert!(folder.raw_provider_json.is_some());

    let normalized = card(
        &json!({
            "id": "contact-1",
            "changeKey": "v2",
            "displayName": "Dr Ada M Lovelace PhD",
            "title": "Dr",
            "givenName": "Ada",
            "middleName": "M",
            "surname": "Lovelace",
            "generation": "PhD",
            "emailAddresses": [
                {"name": "work", "address": "ada@example.test"},
                {"name": "empty", "address": ""}
            ],
            "businessPhones": ["+1-work"],
            "homePhones": ["+1-home"],
            "mobilePhone": "+1-mobile",
            "businessAddress": {
                "street": "1 Main St",
                "city": "London",
                "state": "London",
                "postalCode": "N1",
                "countryOrRegion": "GB"
            },
            "homeAddress": {"city": "Amsterdam", "countryOrRegion": "NL"},
            "otherAddress": {},
            "companyName": "Analytical Engines",
            "department": "Research",
            "jobTitle": "Programmer",
            "personalNotes": "kept note",
            "birthday": "1815-12-10",
            "businessHomePage": "https://example.test",
            "unknown": {"kept": true}
        }),
        AddressBookId::try_from("book").unwrap(),
        ContactSourceClass::Personal,
        true,
    )
    .unwrap();
    assert_eq!(normalized.name.unwrap().components.len(), 5);
    assert_eq!(normalized.emails.len(), 1);
    assert_eq!(normalized.phones.len(), 3);
    assert_eq!(normalized.addresses.len(), 2);
    assert_eq!(normalized.organizations.len(), 1);
    assert_eq!(normalized.titles.len(), 1);
    assert_eq!(normalized.notes.len(), 1);
    assert_eq!(normalized.anniversaries.len(), 1);
    assert_eq!(normalized.urls.len(), 1);
    assert!(normalized.is_writable);
    assert_eq!(normalized.revisions.change_key.unwrap().as_str(), "v2");
    assert!(
        normalized
            .raw_provider_json
            .unwrap()
            .as_str()
            .contains("unknown")
    );
}

#[test]
fn directory_shapes_fall_back_to_mail_and_preserve_guest_kind() {
    let normalized = card(
        &json!({
            "id": "user-1",
            "userType": "Guest",
            "mail": "guest@example.test",
            "mobile": "+1-mobile",
            "notes": "directory note"
        }),
        AddressBookId::try_from("directory").unwrap(),
        ContactSourceClass::Directory,
        false,
    )
    .unwrap();
    assert_eq!(normalized.kind, ContactKind::Other("guest".into()));
    assert_eq!(
        normalized.emails.values().next().unwrap().value.address,
        "guest@example.test"
    );
    assert_eq!(normalized.phones.len(), 1);
    assert_eq!(normalized.notes.len(), 1);
    assert!(normalized.name.is_none());
}

/// A note is not a web address. `personalNotes` was read as a second homepage
/// field, so any note beginning with `http` was republished as a URL resource —
/// duplicated out of the notes it belongs to and into a field a host renders as a
/// link.
#[test]
fn a_note_that_starts_with_a_link_stays_a_note() {
    let normalized = card(
        &json!({
            "id": "contact-2",
            "displayName": "Ada",
            "personalNotes": "https://internal.example/wiki — read before calling",
        }),
        AddressBookId::try_from("book").unwrap(),
        ContactSourceClass::Personal,
        true,
    )
    .unwrap();
    assert!(normalized.urls.is_empty(), "{:?}", normalized.urls);
    assert_eq!(normalized.notes.len(), 1);
}

#[test]
fn proxy_addresses_and_malformed_required_ids_are_handled() {
    let normalized = card(
        &json!({
            "id": "user-2",
            "proxyAddresses": ["SMTP:Primary@Example.test", "malformed", ""],
            "userPrincipalName": "fallback@example.test"
        }),
        AddressBookId::try_from("directory").unwrap(),
        ContactSourceClass::Directory,
        false,
    )
    .unwrap();
    assert_eq!(normalized.emails.len(), 1);
    assert_eq!(
        normalized.emails.values().next().unwrap().value.address,
        "Primary@Example.test"
    );
    assert!(folder(&json!({"displayName": "missing id"})).is_err());
    assert!(
        card(
            &json!({}),
            AddressBookId::try_from("book").unwrap(),
            ContactSourceClass::Personal,
            true,
        )
        .is_err()
    );
}
