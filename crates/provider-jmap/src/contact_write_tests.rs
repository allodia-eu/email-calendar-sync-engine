use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    contact::{
        Anniversary, ContactAddress, ContactEmail, ContactLanguage, ContactMember, ContactName,
        ContactNickname, ContactNote, ContactOnlineService, ContactPhone, ContactProperty,
        ContactRelation, ContactResource, NameComponent, NameComponentKind, Organization,
        PersonalInfo, PropertyId, Title,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::RawJsContact,
    time::UtcDateTime,
};

use super::*;

fn id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

fn card() -> ContactCard {
    let mut card = ContactCard::new(
        ContactId::try_from("card-1").unwrap(),
        Memberships::of_one(AddressBookId::try_from("book-1").unwrap()),
    );
    card.name = Some(ContactName {
        full: Some("Ada Lovelace".into()),
        components: vec![
            NameComponent::new(NameComponentKind::Prefix, "Countess"),
            NameComponent::new(NameComponentKind::Given, "Ada"),
            NameComponent::new(NameComponentKind::Middle, "Byron"),
            NameComponent::new(NameComponentKind::Surname, "Lovelace"),
            NameComponent::new(NameComponentKind::Surname2, "King"),
            NameComponent::new(NameComponentKind::Suffix, "PhD"),
            NameComponent::new(NameComponentKind::Other("x-patronymic".into()), "Noel"),
        ],
        sort_as: BTreeMap::from([("surname".into(), "Lovelace".into())]),
        ..ContactName::default()
    });
    card.uid = Some("urn:uuid:card-1".into());
    card.nicknames.insert(
        id("nickname"),
        ContactProperty::new(ContactNickname::new("Enchantress")),
    );
    let mut email = ContactProperty {
        contexts: BTreeSet::from(["work".into()]),
        preference: Some(1),
        label: Some("Primary".into()),
        ..ContactProperty::new(ContactEmail::new("ada@example.test"))
    };
    email.extensions.set("x-acme", json!({"kept": true}));
    card.emails.insert(id("work"), email);
    card.phones.insert(
        id("mobile"),
        ContactProperty::new(ContactPhone {
            number: "tel:+44123".into(),
            features: BTreeSet::from(["mobile".into()]),
        }),
    );
    let mut address = ContactAddress {
        country_code: Some("GB".into()),
        coordinates: Some("geo:51.5,-0.1".into()),
        time_zone: Some("Europe/London".into()),
        ..ContactAddress::default()
    };
    address
        .components
        .insert("locality".into(), vec!["London".into()]);
    card.addresses
        .insert(id("home"), ContactProperty::new(address));
    card.organizations.insert(
        id("org"),
        ContactProperty::new(Organization {
            name: "Analytical Engines".into(),
            ..Organization::default()
        }),
    );
    card.titles.insert(
        id("title"),
        ContactProperty::new(Title {
            name: "Programmer".into(),
            kind: Some("title".into()),
            organization_id: Some(id("org")),
        }),
    );
    card.anniversaries.insert(
        id("birth"),
        ContactProperty::new(Anniversary {
            date: "1815-12-10".into(),
            kind: Some("birth".into()),
            place: Some("London".into()),
        }),
    );
    card.notes.insert(
        id("note"),
        ContactProperty::new(ContactNote::new("First programmer")),
    );
    card.urls.insert(
        id("site"),
        ContactProperty::new(ContactResource {
            uri: "https://ada.example".into(),
            media_type: Some("text/html".into()),
            ..ContactResource::default()
        }),
    );
    card.media.insert(
        id("photo"),
        ContactProperty::new(ContactResource {
            uri: "https://ada.example/photo".into(),
            kind: Some("photo".into()),
            media_type: Some("image/jpeg".into()),
            fingerprint: Some("blob-1".into()),
            ..ContactResource::default()
        }),
    );
    card.online_services.insert(
        id("social"),
        ContactProperty::new(ContactOnlineService {
            service: Some("Example".into()),
            user: Some("ada".into()),
            uri: Some("https://social.example/ada".into()),
        }),
    );
    card.relations.insert(
        id("relation"),
        ContactProperty::new(ContactRelation {
            relation: BTreeSet::from(["colleague".into()]),
            uid: Some("urn:uuid:babbage".into()),
            uri: None,
        }),
    );
    card.languages.insert(
        id("en"),
        ContactProperty::preferred(ContactLanguage::new("en"), 1),
    );
    card.members.insert(
        id("member"),
        ContactProperty::new(ContactMember::new("urn:uuid:member")),
    );
    card.personal_info.insert(
        id("expertise"),
        ContactProperty::new(PersonalInfo {
            kind: "expertise".into(),
            value: "mathematics".into(),
        }),
    );
    for (target, property, uri) in [
        (
            &mut card.calendars,
            "calendar",
            "webcal://ada.example/calendar",
        ),
        (
            &mut card.scheduling_addresses,
            "schedule",
            "mailto:ada@example.test",
        ),
        (&mut card.crypto_keys, "key", "https://ada.example/key"),
        (
            &mut card.directories,
            "directory",
            "https://ada.example/profile",
        ),
    ] {
        target.insert(
            id(property),
            ContactProperty::new(ContactResource {
                uri: uri.into(),
                ..ContactResource::default()
            }),
        );
    }
    card.keywords.insert("mathematician".into());
    card.created = Some(UtcDateTime::new(2026, 7, 1, 10, 0, 0).unwrap());
    card.updated = Some(UtcDateTime::new(2026, 7, 2, 10, 0, 0).unwrap());
    card
}

/// Raw preservation exists to carry properties this engine does not model — a vendor
/// `x-` extension, a JSContact property added after this version. It must not also
/// carry *modelled* values, or a host that clones a fetched card, edits it, and
/// creates the copy silently ships the values it edited away.
#[test]
fn raw_jscontact_carries_extensions_forward_but_never_overrides_edits() {
    let mut card = card();
    card.raw_jscontact = Some(RawJsContact::new(
        r#"{"id":"server-id","kind":"individual","x-acme":{"kept":true},
            "emails":{"stale":{"@type":"EmailAddress","address":"stale@example.test"}},
            "notes":{"stale":{"@type":"Note","note":"stale"}}}"#,
    ));
    // The edit the host made on the normalized card.
    card.notes.clear();

    let object = writable_object(&card);
    assert!(object.get("id").is_none());
    assert_eq!(object["x-acme"]["kept"], true, "extension must survive");
    assert!(
        object["emails"].get("stale").is_none(),
        "modelled values come from the card, not the raw: {}",
        object["emails"]
    );
    assert_eq!(object["emails"]["work"]["address"], "ada@example.test");
    // A field the host emptied is emptied on the wire too, not restored from raw.
    assert!(object.get("notes").is_none(), "{object:?}");
}

#[test]
fn programmatic_cards_map_all_populated_jscontact_fields() {
    let object = writable_object(&card());
    assert_eq!(object["uid"], "urn:uuid:card-1");
    assert_eq!(object["name"]["components"][0]["kind"], "title");
    assert_eq!(object["name"]["components"][1]["kind"], "given");
    assert_eq!(object["name"]["components"][2]["kind"], "given2");
    assert_eq!(object["name"]["components"][4]["kind"], "surname2");
    assert_eq!(object["name"]["components"][5]["kind"], "credential");
    assert_eq!(object["name"]["components"][6]["kind"], "x-patronymic");
    assert_eq!(object["name"]["sortAs"]["surname"], "Lovelace");
    assert_eq!(object["emails"]["work"]["contexts"]["work"], true);
    assert_eq!(object["emails"]["work"]["pref"], 1);
    assert_eq!(object["emails"]["work"]["label"], "Primary");
    assert_eq!(object["emails"]["work"]["x-acme"]["kept"], true);
    assert_eq!(object["phones"]["mobile"]["features"]["mobile"], true);
    assert_eq!(object["addresses"]["home"]["countryCode"], "GB");
    assert_eq!(object["addresses"]["home"]["timeZone"], "Europe/London");
    assert_eq!(
        object["addresses"]["home"]["components"][0]["kind"],
        "locality"
    );
    assert_eq!(object["organizations"]["org"]["name"], "Analytical Engines");
    assert_eq!(object["titles"]["title"]["organizationId"], "org");
    assert_eq!(object["anniversaries"]["birth"]["kind"], "birth");
    assert_eq!(object["notes"]["note"]["note"], "First programmer");
    assert_eq!(object["links"]["site"]["mediaType"], "text/html");
    assert_eq!(object["media"]["photo"]["blobId"], "blob-1");
    assert_eq!(object["onlineServices"]["social"]["user"], "ada");
    assert_eq!(
        object["relatedTo"]["urn:uuid:babbage"]["relation"]["colleague"],
        true
    );
    assert_eq!(object["preferredLanguages"]["en"]["pref"], 1);
    // RFC 9553 §2.1.7: emitted as String[Boolean] keyed by the member's uid — not as
    // a nested object under a synthesized property id, which no server would read.
    assert_eq!(object["members"]["urn:uuid:member"], true);
    assert_eq!(object["personalInfo"]["expertise"]["kind"], "expertise");
    assert_eq!(
        object["calendars"]["calendar"]["uri"],
        "webcal://ada.example/calendar"
    );
    assert_eq!(
        object["schedulingAddresses"]["schedule"]["uri"],
        "mailto:ada@example.test"
    );
    assert_eq!(
        object["cryptoKeys"]["key"]["uri"],
        "https://ada.example/key"
    );
    assert_eq!(
        object["directories"]["directory"]["uri"],
        "https://ada.example/profile"
    );
    assert_eq!(object["keywords"]["mathematician"], true);
    assert_eq!(object["created"], "2026-07-01T10:00:00Z");
    assert_eq!(object["updated"], "2026-07-02T10:00:00Z");
}

#[test]
fn patches_translate_normalized_values_and_reject_non_jscontact_fields() {
    let card = card();
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(serde_json::to_value(card.name.as_ref().unwrap()).unwrap()),
    );
    patch
        .set_properties(ContactField::Urls, &card.urls)
        .unwrap();
    patch
        .set_properties(ContactField::Languages, &card.languages)
        .unwrap();
    patch
        .fields
        .insert(ContactField::Keywords, FieldPatch::Clear);
    let object = patch_object(&patch).unwrap();
    assert_eq!(object["name"]["sortAs"]["surname"], "Lovelace");
    assert_eq!(object["links"]["site"]["mediaType"], "text/html");
    assert_eq!(object["preferredLanguages"]["en"]["pref"], 1);
    assert!(object["keywords"].is_null());

    let mut unsupported = ContactPatch::default();
    unsupported
        .fields
        .insert(ContactField::TimeZone, FieldPatch::Set(json!("UTC")));
    assert!(patch_object(&unsupported).is_err());
    unsupported.fields.clear();
    unsupported
        .fields
        .insert(ContactField::Kind, FieldPatch::Set(json!("group")));
    assert!(patch_object(&unsupported).is_err());
    assert!(crate::contact_write_fields::field_value(ContactField::Kind, &json!("group")).is_err());

    let clear_kind = ContactPatch {
        kind: Some(FieldPatch::Clear),
        ..ContactPatch::default()
    };
    assert!(patch_object(&clear_kind).unwrap()["kind"].is_null());
}

#[test]
fn every_writable_field_translates_from_typed_patch_intent() {
    let card = card();
    let mut patch = ContactPatch {
        kind: Some(FieldPatch::Set(ContactKind::Organization)),
        ..ContactPatch::default()
    };
    macro_rules! properties {
        ($field:ident, $value:expr) => {
            patch.set_properties(ContactField::$field, $value).unwrap();
        };
    }
    properties!(Nicknames, &card.nicknames);
    properties!(Emails, &card.emails);
    properties!(Phones, &card.phones);
    properties!(Addresses, &card.addresses);
    properties!(Organizations, &card.organizations);
    properties!(Titles, &card.titles);
    properties!(Anniversaries, &card.anniversaries);
    properties!(Notes, &card.notes);
    properties!(Urls, &card.urls);
    properties!(OnlineServices, &card.online_services);
    properties!(Relations, &card.relations);
    properties!(Languages, &card.languages);
    properties!(PersonalInfo, &card.personal_info);
    properties!(Calendars, &card.calendars);
    properties!(SchedulingAddresses, &card.scheduling_addresses);
    properties!(CryptoKeys, &card.crypto_keys);
    properties!(Directories, &card.directories);
    patch.fields.insert(
        ContactField::Keywords,
        FieldPatch::Set(serde_json::to_value(&card.keywords).unwrap()),
    );

    let object = patch_object(&patch).unwrap();
    assert_eq!(object["kind"], "org");
    assert_eq!(object["nicknames"]["nickname"]["name"], "Enchantress");
    assert_eq!(object["emails"]["work"]["address"], "ada@example.test");
    assert_eq!(object["phones"]["mobile"]["number"], "tel:+44123");
    assert_eq!(object["addresses"]["home"]["countryCode"], "GB");
    assert_eq!(object["organizations"]["org"]["name"], "Analytical Engines");
    assert_eq!(object["titles"]["title"]["name"], "Programmer");
    assert_eq!(object["anniversaries"]["birth"]["date"], "1815-12-10");
    assert_eq!(object["notes"]["note"]["note"], "First programmer");
    assert_eq!(object["links"]["site"]["uri"], "https://ada.example");
    assert_eq!(object["onlineServices"]["social"]["user"], "ada");
    assert!(object["relatedTo"]["urn:uuid:babbage"].is_object());
    assert_eq!(object["preferredLanguages"]["en"]["pref"], 1);
    assert_eq!(object["personalInfo"]["expertise"]["value"], "mathematics");
    assert!(object["calendars"]["calendar"].is_object());
    assert!(object["schedulingAddresses"]["schedule"].is_object());
    assert!(object["cryptoKeys"]["key"].is_object());
    assert!(object["directories"]["directory"].is_object());
    assert_eq!(object["keywords"]["mathematician"], true);
}

#[test]
fn every_contact_kind_has_a_stable_jscontact_name() {
    for (kind, expected) in [
        (ContactKind::Individual, "individual"),
        (ContactKind::Organization, "org"),
        (ContactKind::Group, "group"),
        (ContactKind::Location, "location"),
        (ContactKind::Device, "device"),
        (ContactKind::Application, "application"),
        (ContactKind::Other("x-kind".into()), "x-kind"),
    ] {
        assert_eq!(kind_name(&kind), expected);
    }
}
