//! Contact source-model and raw-preservation contracts.

use std::collections::BTreeMap;

use engine_core::{
    contact::{
        AddressBook, Anniversary, ContactAddress, ContactCard, ContactDraft, ContactEmail,
        ContactField, ContactFieldSet, ContactKind, ContactLanguage, ContactName, ContactNickname,
        ContactNote, ContactOnlineService, ContactPatch, ContactPhone, ContactProperty,
        ContactRelation, ContactResource, ContactSourceClass, FieldPatch, NameComponent,
        NameComponentKind, Organization, PersonalInfo, PropertyId, Title,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::{RawJsContact, RawProviderJson, RawVcard},
};

fn property_id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

fn card() -> ContactCard {
    ContactCard::new(
        ContactId::try_from("contact-1").unwrap(),
        Memberships::of_one(AddressBookId::try_from("personal").unwrap()),
    )
}

#[test]
fn contact_ids_are_distinct_and_membership_is_non_empty() {
    let address_book = AddressBookId::try_from("same").unwrap();
    let contact = ContactId::try_from("same").unwrap();
    assert_eq!(address_book.as_str(), contact.as_str());

    let json = serde_json::to_value(card()).unwrap();
    let mut object = json.as_object().unwrap().clone();
    object.insert("address_books".into(), serde_json::json!([]));
    let error = serde_json::from_value::<ContactCard>(object.into()).unwrap_err();
    assert!(error.to_string().contains("at least one"));
}

#[test]
fn card_roundtrip_preserves_property_ids_and_raw_documents() {
    let mut card = card();
    card.uid = Some("urn:uuid:contact-1".into());
    card.kind = ContactKind::Individual;
    card.name = Some(ContactName {
        full: Some("山田 太郎".into()),
        components: vec![
            NameComponent::new(NameComponentKind::Surname, "山田"),
            NameComponent::new(NameComponentKind::Given, "太郎"),
        ],
        ..ContactName::default()
    });
    card.emails.insert(
        property_id("work"),
        ContactProperty::preferred(ContactEmail::new("Taro@例え.テスト"), 1),
    );
    card.raw_vcard = Some(RawVcard::new(include_str!(
        "fixtures/contacts/complete-card.vcf"
    )));
    card.raw_jscontact = Some(RawJsContact::new(include_str!(
        "fixtures/contacts/complete-card.json"
    )));
    card.raw_provider_json = Some(RawProviderJson::new(
        r#"{"unknown":{"order":["is","preserved"]}}"#,
    ));

    let encoded = serde_json::to_string(&card).unwrap();
    let decoded: ContactCard = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, card);
    assert_eq!(
        decoded.emails[&property_id("work")].value.address,
        "Taro@例え.テスト"
    );
    assert!(decoded.raw_vcard.unwrap().as_str().contains("X-ACME"));
    assert!(
        decoded
            .raw_provider_json
            .unwrap()
            .as_str()
            .contains("\"order\"")
    );
}

#[test]
fn raw_contact_debug_output_is_redacted() {
    for shown in [
        format!("{:?}", RawVcard::new("NOTE:secret")),
        format!("{:?}", RawJsContact::new(r#"{"note":"secret"}"#)),
        format!("{:?}", RawProviderJson::new(r#"{"note":"secret"}"#)),
    ] {
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("len"), "{shown}");
    }
}

#[test]
fn address_book_records_source_and_access_without_implying_identity() {
    let mut book = AddressBook::new(
        AddressBookId::try_from("directory").unwrap(),
        "Company Directory",
        ContactSourceClass::Directory,
    );
    book.is_writable = false;
    book.supported_fields = BTreeMap::from([("email".into(), true)]);

    let roundtrip: AddressBook =
        serde_json::from_str(&serde_json::to_string(&book).unwrap()).unwrap();
    assert_eq!(roundtrip, book);
    assert!(!roundtrip.is_writable);
}

#[test]
fn contact_name_falls_back_to_non_blank_components() {
    let name = ContactName {
        full: Some("  ".into()),
        components: vec![
            NameComponent::new(NameComponentKind::Prefix, ""),
            NameComponent::new(NameComponentKind::Given, "Ada"),
            NameComponent::new(NameComponentKind::Surname, "Lovelace"),
        ],
        ..ContactName::default()
    };
    assert_eq!(name.display().as_deref(), Some("Ada Lovelace"));
    assert_eq!(ContactName::default().display(), None);
    assert_eq!(ContactCard::default().display_name(), None);
}

#[test]
fn populated_and_requested_fields_cover_every_writable_card_field() {
    let mut card = card();
    card.name = Some(ContactName {
        full: Some("Complete Contact".into()),
        ..ContactName::default()
    });
    card.nicknames.insert(
        property_id("nickname"),
        ContactProperty::new(ContactNickname::new("C")),
    );
    card.emails.insert(
        property_id("email"),
        ContactProperty::new(ContactEmail::new("c@example.test")),
    );
    card.phones.insert(
        property_id("phone"),
        ContactProperty::new(ContactPhone::default()),
    );
    card.addresses.insert(
        property_id("address"),
        ContactProperty::new(ContactAddress::default()),
    );
    card.organizations.insert(
        property_id("organization"),
        ContactProperty::new(Organization::default()),
    );
    card.titles
        .insert(property_id("title"), ContactProperty::new(Title::default()));
    card.anniversaries.insert(
        property_id("anniversary"),
        ContactProperty::new(Anniversary::default()),
    );
    card.notes.insert(
        property_id("note"),
        ContactProperty::new(ContactNote::new("note")),
    );
    card.urls.insert(
        property_id("url"),
        ContactProperty::new(ContactResource::default()),
    );
    card.online_services.insert(
        property_id("service"),
        ContactProperty::new(ContactOnlineService::default()),
    );
    card.relations.insert(
        property_id("relation"),
        ContactProperty::new(ContactRelation::default()),
    );
    card.languages.insert(
        property_id("language"),
        ContactProperty::new(ContactLanguage::new("en")),
    );
    card.personal_info.insert(
        property_id("personal"),
        ContactProperty::new(PersonalInfo::default()),
    );
    for (id, field) in [
        ("calendar", ContactField::Calendars),
        ("scheduling", ContactField::SchedulingAddresses),
        ("key", ContactField::CryptoKeys),
        ("directory", ContactField::Directories),
    ] {
        let target = match field {
            ContactField::Calendars => &mut card.calendars,
            ContactField::SchedulingAddresses => &mut card.scheduling_addresses,
            ContactField::CryptoKeys => &mut card.crypto_keys,
            ContactField::Directories => &mut card.directories,
            _ => unreachable!(),
        };
        target.insert(
            property_id(id),
            ContactProperty::new(ContactResource::default()),
        );
    }
    card.keywords.insert("friend".into());
    card.time_zone = Some("Europe/Amsterdam".into());

    let populated = card.populated_fields();
    let expected = ContactFieldSet::from_fields([
        ContactField::Kind,
        ContactField::Name,
        ContactField::Nicknames,
        ContactField::Emails,
        ContactField::Phones,
        ContactField::Addresses,
        ContactField::Organizations,
        ContactField::Titles,
        ContactField::Anniversaries,
        ContactField::Notes,
        ContactField::Urls,
        ContactField::OnlineServices,
        ContactField::Relations,
        ContactField::Languages,
        ContactField::PersonalInfo,
        ContactField::Calendars,
        ContactField::SchedulingAddresses,
        ContactField::CryptoKeys,
        ContactField::Directories,
        ContactField::Keywords,
        ContactField::TimeZone,
    ]);
    assert_eq!(populated, expected);
    assert!(expected.contains_all(&populated));
    assert!(expected.contains(ContactField::Emails));
    assert_eq!(expected.iter().count(), 21);

    let draft = ContactDraft {
        address_book: AddressBookId::try_from("personal").unwrap(),
        card,
    };
    assert_eq!(draft.requested_fields(), expected);
}

#[test]
fn contact_patch_exposes_kind_fields_and_typed_property_replacements() {
    let mut patch = ContactPatch {
        kind: Some(FieldPatch::Set(ContactKind::Organization)),
        ..ContactPatch::default()
    };
    let emails = BTreeMap::from([(
        property_id("work"),
        ContactProperty::new(ContactEmail::new("work@example.test")),
    )]);
    patch.set_properties(ContactField::Emails, &emails).unwrap();
    patch.fields.insert(ContactField::Notes, FieldPatch::Clear);

    let requested = patch.requested_fields();
    assert_eq!(
        requested,
        ContactFieldSet::from_fields([
            ContactField::Kind,
            ContactField::Emails,
            ContactField::Notes,
        ])
    );
    let roundtrip: ContactPatch =
        serde_json::from_str(&serde_json::to_string(&patch).unwrap()).unwrap();
    assert_eq!(roundtrip, patch);
}

#[test]
fn property_ids_validate_and_deserialize() {
    assert!(PropertyId::new("").is_err());
    assert_eq!(property_id("stable").as_str(), "stable");
    assert!(serde_json::from_str::<PropertyId>("\"\"").is_err());
}
