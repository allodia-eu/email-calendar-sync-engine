use engine_core::{
    contact::{
        ContactCard, ContactEmail, ContactField, ContactKind, ContactName, ContactNote,
        ContactPatch, ContactPhone, ContactProperty, ContactResource, FieldPatch, NameComponent,
        NameComponentKind, PropertyId,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::RawVcard,
};
use serde_json::json;

use crate::{
    vcard::parse_vcard,
    vcard_write::{build_vcard, patch_vcard},
};

#[test]
fn parses_multi_email_group_and_preserves_unknown_lines() {
    let raw = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:group-1\r\nKIND:group\r\nFN:International Friends\r\nEMAIL;PROP-ID=team;TYPE=work;PREF=1:Team@BÜCHER.example\r\nEMAIL;TYPE=home:friends@example.net\r\nMEMBER:urn:uuid:alice\r\nX-AB-CUSTOM:keep-me\r\nEND:VCARD\r\n";
    let card = parse_vcard(
        raw,
        ContactId::try_from("/contacts/group-1.vcf").unwrap(),
        AddressBookId::try_from("/contacts/").unwrap(),
        true,
    )
    .unwrap();
    assert_eq!(card.kind, ContactKind::Group);
    assert_eq!(card.emails.len(), 2);
    assert!(card.emails.keys().any(|id| id.as_str() == "team"));
    assert_eq!(card.members.len(), 1);
    assert_eq!(card.raw_vcard.as_ref().map(RawVcard::as_str), Some(raw));
}

fn parse_fixture(name: &str) -> ContactCard {
    let raw = match name {
        "complete" => include_str!("../../engine-core/tests/fixtures/contacts/complete-card.vcf"),
        "group" => include_str!("../../engine-core/tests/fixtures/contacts/group.vcf"),
        "legacy" => {
            include_str!("../../engine-core/tests/fixtures/contacts/legacy-malformed.vcf")
        }
        _ => unreachable!(),
    };
    let id = format!("/contacts/{name}.vcf");
    parse_vcard(
        raw,
        ContactId::try_from(id.as_str()).unwrap(),
        AddressBookId::try_from("/contacts/").unwrap(),
        true,
    )
    .unwrap()
}

#[test]
fn comprehensive_international_vcard_maps_supported_fields() {
    let card = parse_fixture("complete");
    assert_eq!(card.kind, ContactKind::Individual);
    assert_eq!(card.name.as_ref().unwrap().components.len(), 2);
    assert_eq!(card.nicknames.len(), 1);
    assert_eq!(card.emails.len(), 2);
    assert_eq!(card.emails.values().next().unwrap().preference, Some(1));
    assert_eq!(card.phones.len(), 1);
    assert!(
        card.phones
            .values()
            .next()
            .unwrap()
            .value
            .features
            .contains("cell")
    );
    assert_eq!(card.addresses.len(), 1);
    assert_eq!(card.organizations.len(), 1);
    assert_eq!(card.titles.len(), 1);
    assert_eq!(card.notes.len(), 1);
    assert_eq!(card.media.len(), 1);
    assert!(card.raw_vcard.unwrap().as_str().contains("X-ACME-PROFILE"));
}

#[test]
fn group_and_malformed_legacy_cards_remain_syncable_and_lossless() {
    let group = parse_fixture("group");
    assert_eq!(group.kind, ContactKind::Group);
    assert_eq!(group.members.len(), 2);

    let legacy = parse_fixture("legacy");
    assert_eq!(legacy.emails.len(), 1);
    assert_eq!(legacy.addresses.len(), 1);
    let photo = legacy.media.values().next().unwrap();
    assert!(photo.value.uri.starts_with("data:image/jpeg;base64,"));
    assert!(
        legacy
            .raw_vcard
            .unwrap()
            .as_str()
            .contains("X-LEGACY-UNKNOWN")
    );
    assert!(
        parse_vcard(
            "VERSION:4.0\r\nFN:Missing wrapper\r\n",
            ContactId::try_from("bad").unwrap(),
            AddressBookId::try_from("book").unwrap(),
            false,
        )
        .is_err()
    );

    let raw = "BEGIN:VCARD\r\nVERSION:4.0\r\nBROKEN\r\nBDAY:1815-12-10\r\nANNIVERSARY:1835-07-08\r\nEND:VCARD\r\n";
    let dated = parse_vcard(
        raw,
        ContactId::try_from("dated").unwrap(),
        AddressBookId::try_from("book").unwrap(),
        false,
    )
    .unwrap();
    assert_eq!(dated.anniversaries.len(), 2);

    for (kind, expected) in [
        ("org", ContactKind::Organization),
        ("location", ContactKind::Location),
        ("device", ContactKind::Device),
        ("application", ContactKind::Application),
        ("x-kind", ContactKind::Other("x-kind".into())),
    ] {
        let raw = format!("BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:{kind}\r\nEND:VCARD\r\n");
        assert_eq!(
            parse_vcard(
                &raw,
                ContactId::try_from(format!("kind-{kind}").as_str()).unwrap(),
                AddressBookId::try_from("book").unwrap(),
                false,
            )
            .unwrap()
            .kind,
            expected
        );
    }
}

fn id(value: &str) -> PropertyId {
    PropertyId::new(value).unwrap()
}

fn writable_card() -> ContactCard {
    let book = AddressBookId::try_from("/contacts/").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("/contacts/ada.vcf").unwrap(),
        Memberships::of_one(book),
    );
    card.uid = Some("ada".into());
    card.kind = ContactKind::Individual;
    card.name = Some(ContactName {
        full: Some("Ada Lovelace".into()),
        ..ContactName::default()
    });
    card.emails.insert(
        id("email"),
        ContactProperty::new(ContactEmail::new("ada@example.test")),
    );
    card.phones.insert(
        id("phone"),
        ContactProperty::new(ContactPhone {
            number: "+44 123".into(),
            ..ContactPhone::default()
        }),
    );
    card.notes.insert(
        id("note"),
        ContactProperty::new(ContactNote::new("First programmer")),
    );
    card.urls.insert(
        id("url"),
        ContactProperty::new(ContactResource {
            uri: "https://ada.example".into(),
            ..ContactResource::default()
        }),
    );
    card.keywords
        .extend(["mathematician".into(), "programmer".into()]);
    card
}

#[test]
fn create_vcard_includes_every_advertised_field() {
    let raw = build_vcard(&writable_card());
    for expected in [
        "UID:ada",
        "KIND:individual",
        "FN:Ada Lovelace",
        "EMAIL:ada@example.test",
        "TEL:+44 123",
        "NOTE:First programmer",
        "URL:https://ada.example",
        "CATEGORIES:mathematician,programmer",
    ] {
        assert!(raw.contains(expected), "{raw}");
    }
}

#[test]
fn raw_preserving_patch_sets_clears_and_rejects_malformed_fields() {
    let mut base = writable_card();
    base.raw_vcard = Some(RawVcard::new(
        "BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:individual\r\nFN:Old\r\nEMAIL:old@example.test\r\nX-KEEP:yes\r\nEND:VCARD\r\n",
    ));
    let replacement = writable_card();
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(serde_json::to_value(replacement.name.unwrap()).unwrap()),
    );
    patch
        .set_properties(ContactField::Notes, &replacement.notes)
        .unwrap();
    patch
        .set_properties(ContactField::Emails, &replacement.emails)
        .unwrap();
    patch
        .set_properties(ContactField::Phones, &replacement.phones)
        .unwrap();
    patch
        .set_properties(ContactField::Urls, &replacement.urls)
        .unwrap();
    patch.fields.insert(
        ContactField::Keywords,
        FieldPatch::Set(serde_json::to_value(&replacement.keywords).unwrap()),
    );
    patch.kind = Some(FieldPatch::Set(ContactKind::Organization));
    let raw = patch_vcard(&base, &patch).unwrap();
    assert!(raw.contains("FN:Ada Lovelace"));
    assert!(raw.contains("NOTE:First programmer"));
    assert!(raw.contains("EMAIL:ada@example.test"));
    assert!(raw.contains("TEL:+44 123"));
    assert!(raw.contains("URL:https://ada.example"));
    assert!(raw.contains("CATEGORIES:mathematician,programmer"));
    assert!(raw.contains("KIND:org"));
    assert!(raw.contains("X-KEEP:yes"));

    let mut malformed = ContactPatch::default();
    malformed
        .fields
        .insert(ContactField::Name, FieldPatch::Set(json!("bad")));
    assert!(patch_vcard(&base, &malformed).is_err());
    let mut unsupported = ContactPatch::default();
    unsupported
        .fields
        .insert(ContactField::Addresses, FieldPatch::Clear);
    assert!(patch_vcard(&base, &unsupported).is_err());
    let no_raw = writable_card();
    assert!(patch_vcard(&no_raw, &ContactPatch::default()).is_err());

    for kind in [
        ContactKind::Individual,
        ContactKind::Group,
        ContactKind::Location,
        ContactKind::Device,
        ContactKind::Application,
        ContactKind::Other("x-kind".into()),
    ] {
        let patch = ContactPatch {
            kind: Some(FieldPatch::Set(kind)),
            ..ContactPatch::default()
        };
        assert!(patch_vcard(&base, &patch).unwrap().contains("KIND:"));
    }
    let clear_kind = ContactPatch {
        kind: Some(FieldPatch::Clear),
        ..ContactPatch::default()
    };
    assert!(!patch_vcard(&base, &clear_kind).unwrap().contains("KIND:"));
}

/// `ContactKind::Other` carries host-supplied text, so it is as untrusted as any
/// other written value: a card whose kind embeds a line break must not be able to
/// smuggle extra properties into the `PUT` body.
#[test]
fn a_hostile_contact_kind_cannot_inject_vcard_properties() {
    let hostile = ContactKind::Other("individual\r\nEMAIL:attacker@evil.test".into());
    let mut card = writable_card();
    card.kind = hostile.clone();
    let created = build_vcard(&card);
    // The hostile text survives as *data* on the KIND line; what it must never do is
    // start a content line of its own.
    assert!(created.contains("KIND:individual\\nEMAIL:attacker@evil.test"));
    assert!(
        !created
            .lines()
            .any(|line| line.starts_with("EMAIL:attacker@evil.test")),
        "{created}"
    );

    let mut base = writable_card();
    base.raw_vcard = Some(RawVcard::new(
        "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Old\r\nEND:VCARD\r\n",
    ));
    let patch = ContactPatch {
        kind: Some(FieldPatch::Set(hostile)),
        ..ContactPatch::default()
    };
    let patched = patch_vcard(&base, &patch).unwrap();
    assert!(
        !patched
            .lines()
            .any(|line| line.starts_with("EMAIL:attacker@evil.test")),
        "{patched}"
    );
}

/// `patch_vcard` strips both `FN` and `N` for a name edit, so it owes the card a
/// replacement `N`: dropping it silently deletes the structured name the server
/// held, and the next sync reports the contact as having no name components.
#[test]
fn a_name_edit_rewrites_the_structured_name_it_replaced() {
    let mut base = writable_card();
    base.raw_vcard = Some(RawVcard::new(
        "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Old Name\r\nN:Old;Name;;;\r\nEND:VCARD\r\n",
    ));
    let name = ContactName {
        full: Some("Ada Lovelace".into()),
        components: vec![
            NameComponent::new(NameComponentKind::Given, "Ada"),
            NameComponent::new(NameComponentKind::Surname, "Lovelace"),
            NameComponent::new(NameComponentKind::Suffix, "Countess"),
        ],
        ..ContactName::default()
    };
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(serde_json::to_value(&name).unwrap()),
    );
    let raw = patch_vcard(&base, &patch).unwrap();
    assert!(raw.contains("FN:Ada Lovelace"), "{raw}");
    assert!(raw.contains("N:Lovelace;Ada;;;Countess"), "{raw}");
    assert!(!raw.contains("N:Old;Name"), "{raw}");

    // Round-trips: what the writer emits is what the reader recovers, re-ordered into
    // the fixed `N` slot order that vCard — not the model — dictates.
    let reparsed = parse_vcard(
        &raw,
        ContactId::try_from("/contacts/ada.vcf").unwrap(),
        AddressBookId::try_from("/contacts/").unwrap(),
        true,
    )
    .unwrap();
    assert_eq!(
        reparsed.name.unwrap().components,
        vec![
            NameComponent::new(NameComponentKind::Surname, "Lovelace"),
            NameComponent::new(NameComponentKind::Given, "Ada"),
            NameComponent::new(NameComponentKind::Suffix, "Countess"),
        ]
    );
}

/// The create path owes the same `N` line; a draft's components would otherwise be
/// dropped on the way to the server.
#[test]
fn create_writes_structured_name_components_and_escapes_separators() {
    let mut card = writable_card();
    card.name = Some(ContactName {
        full: Some("Ada Lovelace".into()),
        components: vec![
            NameComponent::new(NameComponentKind::Surname, "King;Noel"),
            NameComponent::new(NameComponentKind::Given, "Ada"),
        ],
        ..ContactName::default()
    });
    let raw = build_vcard(&card);
    assert!(raw.contains("N:King\\;Noel;Ada;;;"), "{raw}");
    let reparsed = parse_vcard(
        &raw,
        ContactId::try_from("/contacts/ada.vcf").unwrap(),
        AddressBookId::try_from("/contacts/").unwrap(),
        true,
    )
    .unwrap();
    let components = reparsed.name.unwrap().components;
    assert_eq!(components[0].value, "King;Noel");
    assert_eq!(components[1].value, "Ada");

    // A name with no components leaves no stale `N` behind.
    let mut plain = writable_card();
    plain.name = Some(ContactName {
        full: Some("Ada".into()),
        ..ContactName::default()
    });
    assert!(!build_vcard(&plain).contains("\r\nN:"));
}
