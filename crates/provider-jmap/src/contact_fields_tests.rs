use serde_json::json;

use super::*;

fn id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

#[test]
fn comprehensive_jscontact_fields_and_extensions_are_normalized() {
    let value = json!({
        "nicknames": {"nick": {"name": "Ada", "contexts": {"private": true}}},
        "phones": {"mobile": {"number": "tel:+44123", "features": {"mobile": true}}},
        "addresses": {"home": {
            "full": "1 Main St",
            "components": [
                {"kind": "street", "value": "1 Main St"},
                {"kind": "locality", "value": "London"}
            ],
            "countryCode": "GB",
            "timeZone": "Europe/London"
        }},
        "organizations": {"org": {"name": "Engines", "units": [{"name": "Research"}]}},
        "titles": {"title": {"name": "Programmer", "organizationId": "org"}},
        "anniversaries": {"birth": {
            "date": "1815-12-10",
            "kind": "birth",
            "place": {"full": "London"}
        }},
        "notes": {"note": {"note": "First programmer"}},
        "links": {"site": {"uri": "https://ada.example", "kind": "contact"}},
        "media": {"photo": {
            "uri": "https://ada.example/photo",
            "mediaType": "image/jpeg",
            "blobId": "blob-1"
        }},
        "onlineServices": {"social": {
            "service": "Example",
            "user": "ada",
            "uri": "https://social.example/ada"
        }},
        "relatedTo": {"urn:uuid:babbage": {"relation": {"colleague": true}}},
        "preferredLanguages": {"en": {"pref": 1}},
        "personalInfo": {"expertise": {"kind": "expertise", "value": "mathematics"}},
        "calendars": {"calendar": {"uri": "webcal://ada.example/calendar"}},
        "schedulingAddresses": {"schedule": {"uri": "mailto:ada@example.test"}},
        "cryptoKeys": {"key": {"uri": "https://ada.example/key"}},
        "directories": {"directory": {"uri": "https://ada.example/profile"}},
        "keywords": {"mathematician": true, "ignored": false},
        "created": "2026-07-01T10:00:00Z",
        "updated": "2026-07-02T10:00:00Z",
        "x-acme-profile": {"kept": true}
    });
    let mut card = ContactCard::default();
    apply(&mut card, &value).unwrap();
    assert_eq!(card.nicknames.len(), 1);
    assert!(
        card.phones
            .get(&id("mobile"))
            .unwrap()
            .value
            .features
            .contains("mobile")
    );
    assert_eq!(
        card.addresses.get(&id("home")).unwrap().value.components["locality"],
        ["London"]
    );
    assert_eq!(card.organizations.len(), 1);
    assert_eq!(
        card.titles
            .get(&id("title"))
            .unwrap()
            .value
            .organization_id
            .as_ref()
            .unwrap()
            .as_str(),
        "org"
    );
    assert_eq!(card.anniversaries.len(), 1);
    assert_eq!(
        card.anniversaries
            .get(&id("birth"))
            .unwrap()
            .value
            .place
            .as_deref(),
        Some("London")
    );
    assert_eq!(card.notes.len(), 1);
    assert_eq!(card.urls.len(), 1);
    assert_eq!(
        card.media
            .get(&id("photo"))
            .unwrap()
            .value
            .fingerprint
            .as_deref(),
        Some("blob-1")
    );
    assert_eq!(card.online_services.len(), 1);
    assert_eq!(card.relations.len(), 1);
    assert_eq!(card.languages.get(&id("en")).unwrap().value.language, "en");
    assert_eq!(card.personal_info.len(), 1);
    assert_eq!(card.calendars.len(), 1);
    assert_eq!(card.scheduling_addresses.len(), 1);
    assert_eq!(card.crypto_keys.len(), 1);
    assert_eq!(card.directories.len(), 1);
    assert_eq!(card.keywords, ["mathematician".to_owned()].into());
    assert!(card.created.is_some());
    assert!(card.updated.is_some());
    assert_eq!(
        card.extended.get("jscontact/x-acme-profile"),
        Some(&json!({"kept": true}))
    );
}

#[test]
fn malformed_property_ids_and_timestamps_are_rejected() {
    let mut card = ContactCard::default();
    assert!(apply(&mut card, &json!({"nicknames": {"": {"name": "bad"}}})).is_err());
    assert!(apply(&mut card, &json!({"created": "not-a-date"})).is_err());
    apply(
        &mut card,
        &json!({"addresses": {"home": {
            "components": [{"kind": "street"}, {"kind": "locality", "value": "London"}]
        }}}),
    )
    .unwrap();
    assert_eq!(
        card.addresses.get(&id("home")).unwrap().value.components["locality"],
        ["London"]
    );
}
