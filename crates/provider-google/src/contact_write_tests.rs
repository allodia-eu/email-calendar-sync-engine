use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    contact::{
        Anniversary, ContactAddress, ContactEmail, ContactName, ContactNickname, ContactNote,
        ContactPhone, ContactProperty, ContactRelation, ContactResource, NameComponent,
        NameComponentKind, Organization, OrganizationUnit, PropertyId, Title,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::RawProviderJson,
    version::{ETag, RevisionTokens},
};

use super::*;

fn id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

fn comprehensive_card() -> ContactCard {
    let book = AddressBookId::try_from("google-connections").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("people/c1").unwrap(),
        Memberships::of_one(book),
    );
    card.name = Some(ContactName {
        full: Some("Dr Ada Lovelace".into()),
        components: vec![
            NameComponent::new(NameComponentKind::Prefix, "Dr"),
            NameComponent::new(NameComponentKind::Given, "Ada"),
            NameComponent::new(NameComponentKind::Middle, "M"),
            NameComponent::new(NameComponentKind::Surname, "Lovelace"),
            NameComponent::new(NameComponentKind::Suffix, "PhD"),
            NameComponent::new(NameComponentKind::Other("x-extra".into()), "ignored"),
        ],
        ..ContactName::default()
    });
    card.nicknames.insert(
        id("nickname"),
        ContactProperty::new(ContactNickname::new("Enchantress")),
    );
    card.emails.insert(
        id("work"),
        ContactProperty {
            contexts: BTreeSet::from(["work".into()]),
            preference: Some(1),
            ..ContactProperty::new(ContactEmail::new("ada@example.test"))
        },
    );
    card.phones.insert(
        id("mobile"),
        ContactProperty {
            contexts: BTreeSet::from(["private".into()]),
            ..ContactProperty::new(ContactPhone {
                number: "+44 123".into(),
                features: BTreeSet::from(["mobile".into()]),
            })
        },
    );
    card.addresses.insert(
        id("home"),
        ContactProperty {
            contexts: BTreeSet::from(["private".into()]),
            ..ContactProperty::new(ContactAddress {
                full: Some("1 Main St, London".into()),
                components: BTreeMap::from([
                    ("street".into(), vec!["1 Main St".into()]),
                    ("locality".into(), vec!["London".into()]),
                    ("region".into(), vec!["London".into()]),
                    ("postcode".into(), vec!["N1".into()]),
                    ("country".into(), vec!["United Kingdom".into()]),
                ]),
                country_code: Some("GB".into()),
                ..ContactAddress::default()
            })
        },
    );
    card.organizations.insert(
        id("org"),
        ContactProperty::new(Organization {
            name: "Analytical Engines".into(),
            units: vec![OrganizationUnit {
                name: "Research".into(),
                ..OrganizationUnit::default()
            }],
            ..Organization::default()
        }),
    );
    card.titles.insert(
        id("title"),
        ContactProperty::new(Title {
            name: "Programmer".into(),
            kind: Some("title".into()),
            organization_id: None,
        }),
    );
    card.anniversaries.insert(
        id("birthday"),
        ContactProperty::new(Anniversary {
            date: "1815-12-10".into(),
            kind: Some("birth".into()),
            place: None,
        }),
    );
    card.notes.insert(
        id("note"),
        ContactProperty::new(ContactNote::new("First programmer")),
    );
    card.urls.insert(
        id("site"),
        ContactProperty::new(ContactResource {
            uri: "https://ada.example.test".into(),
            ..ContactResource::default()
        }),
    );
    card.relations.insert(
        id("manager"),
        ContactProperty::new(ContactRelation {
            relation: BTreeSet::from(["manager".into()]),
            uid: Some("Charles Babbage".into()),
            uri: None,
        }),
    );
    card.keywords.insert("mathematician".into());
    card
}

#[test]
fn create_maps_every_advertised_google_field() {
    let card = comprehensive_card();
    let body: Value = serde_json::from_slice(
        &create_body(&ContactDraft {
            address_book: AddressBookId::try_from("google-connections").unwrap(),
            card,
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(body["names"][0]["givenName"], "Ada");
    assert_eq!(body["nicknames"][0]["value"], "Enchantress");
    assert_eq!(body["emailAddresses"][0]["metadata"]["primary"], true);
    assert_eq!(body["phoneNumbers"][0]["type"], "mobile");
    assert_eq!(body["addresses"][0]["countryCode"], "GB");
    assert_eq!(body["organizations"][0]["department"], "Research");
    assert_eq!(body["organizations"][0]["title"], "Programmer");
    assert_eq!(body["birthdays"][0]["date"]["year"], 1815);
    assert_eq!(body["biographies"][0]["value"], "First programmer");
    assert_eq!(body["urls"][0]["value"], "https://ada.example.test");
    assert_eq!(body["relations"][0]["person"], "Charles Babbage");
    assert_eq!(body["userDefined"][0]["value"], "mathematician");
}

#[test]
fn patch_decodes_normalized_values_preserves_raw_and_deduplicates_masks() {
    let mut base = comprehensive_card();
    base.revisions = RevisionTokens::from_etag(ETag::new("etag-1"));
    base.raw_provider_json = Some(RawProviderJson::new(
        r#"{"resourceName":"people/c1","unknown":{"kept":true},"organizations":[]}"#,
    ));
    let mut patch = ContactPatch::default();
    patch
        .set_properties(ContactField::Organizations, &base.organizations)
        .unwrap();
    patch
        .set_properties(ContactField::Titles, &base.titles)
        .unwrap();
    patch
        .set_properties(ContactField::Notes, &base.notes)
        .unwrap();
    patch.fields.insert(ContactField::Emails, FieldPatch::Clear);
    let body: Value = serde_json::from_slice(&patch_body(&base, &patch).unwrap()).unwrap();
    assert_eq!(body["unknown"]["kept"], true);
    assert_eq!(body["etag"], "etag-1");
    assert_eq!(body["organizations"][0]["name"], "Analytical Engines");
    assert_eq!(body["organizations"][0]["title"], "Programmer");
    assert_eq!(body["biographies"][0]["value"], "First programmer");
    assert_eq!(body["emailAddresses"], json!([]));
    assert_eq!(
        update_fields(&patch).unwrap(),
        "biographies,emailAddresses,organizations"
    );
}

#[test]
fn malformed_and_unsupported_intent_is_rejected() {
    let base = comprehensive_card();
    let mut malformed = ContactPatch::default();
    malformed
        .fields
        .insert(ContactField::Name, FieldPatch::Set(json!("not a name")));
    assert!(patch_body(&base, &malformed).is_err());

    let mut unsupported = ContactPatch::default();
    unsupported
        .fields
        .insert(ContactField::OnlineServices, FieldPatch::Set(json!({})));
    assert!(patch_body(&base, &unsupported).is_err());

    let mut wrong_kind = ContactPatch {
        kind: Some(FieldPatch::Set(engine_core::contact::ContactKind::Group)),
        ..ContactPatch::default()
    };
    assert!(patch_body(&base, &wrong_kind).is_err());
    wrong_kind.kind = Some(FieldPatch::Set(
        engine_core::contact::ContactKind::Individual,
    ));
    assert_eq!(update_fields(&wrong_kind).unwrap(), "");

    let mut draft = ContactDraft {
        address_book: AddressBookId::try_from("google-connections").unwrap(),
        card: base,
    };
    draft.card.kind = engine_core::contact::ContactKind::Group;
    assert!(create_body(&draft).is_err());
    assert!(update_fields(&unsupported).is_err());
}

#[test]
fn every_supported_patch_field_uses_normalized_intent() {
    let base = comprehensive_card();
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(serde_json::to_value(base.name.as_ref().unwrap()).unwrap()),
    );
    patch
        .set_properties(ContactField::Nicknames, &base.nicknames)
        .unwrap();
    patch
        .set_properties(ContactField::Emails, &base.emails)
        .unwrap();
    patch
        .set_properties(ContactField::Phones, &base.phones)
        .unwrap();
    patch
        .set_properties(ContactField::Addresses, &base.addresses)
        .unwrap();
    patch
        .set_properties(ContactField::Anniversaries, &base.anniversaries)
        .unwrap();
    patch
        .set_properties(ContactField::Notes, &base.notes)
        .unwrap();
    patch
        .set_properties(ContactField::Urls, &base.urls)
        .unwrap();
    patch
        .set_properties(ContactField::Relations, &base.relations)
        .unwrap();
    patch.fields.insert(
        ContactField::Keywords,
        FieldPatch::Set(serde_json::to_value(&base.keywords).unwrap()),
    );
    patch
        .fields
        .insert(ContactField::Organizations, FieldPatch::Clear);
    let body: Value = serde_json::from_slice(&patch_body(&base, &patch).unwrap()).unwrap();
    assert_eq!(body["names"][0]["middleName"], "M");
    assert_eq!(body["names"][0]["honorificSuffix"], "PhD");
    assert_eq!(body["nicknames"][0]["value"], "Enchantress");
    assert_eq!(body["addresses"][0]["city"], "London");
    assert_eq!(body["birthdays"][0]["date"]["day"], 10);
    assert_eq!(body["organizations"][0]["title"], "Programmer");
    assert_eq!(body["userDefined"][0]["key"], "category");
}

#[test]
fn invalid_or_unrepresentable_birthdays_are_rejected() {
    for anniversary in [
        Anniversary {
            date: "2026-01-01".into(),
            kind: Some("wedding".into()),
            place: None,
        },
        Anniversary {
            date: "not-a-date".into(),
            kind: Some("birth".into()),
            place: None,
        },
        Anniversary {
            date: "2026-01".into(),
            kind: None,
            place: None,
        },
    ] {
        let mut card = comprehensive_card();
        card.anniversaries.clear();
        card.anniversaries
            .insert(id("date"), ContactProperty::new(anniversary));
        let draft = ContactDraft {
            address_book: AddressBookId::try_from("google-connections").unwrap(),
            card,
        };
        assert!(create_body(&draft).is_err());
    }
}
