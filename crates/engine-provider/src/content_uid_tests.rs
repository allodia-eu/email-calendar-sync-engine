use engine_core::{
    contact::{
        Anniversary, ContactCard, ContactDraft, ContactEmail, ContactName, ContactProperty,
        PropertyId,
    },
    ids::{AddressBookId, ContactId, Uid},
    membership::Memberships,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

fn draft() -> ContactDraft {
    let book = AddressBookId::try_from("book-1").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("ignored").unwrap(),
        Memberships::of_one(book.clone()),
    );
    card.name = Some(ContactName {
        full: Some("Ada Lovelace".into()),
        ..ContactName::default()
    });
    card.emails.insert(
        PropertyId::new("email").unwrap(),
        ContactProperty::new(ContactEmail::new("ada@example.test")),
    );
    ContactDraft {
        address_book: book,
        card,
    }
}

#[test]
fn the_uid_is_a_uuidv8_of_the_digest() {
    let minted = content_uid_of(&draft());
    let uuid_text = minted
        .uid()
        .as_str()
        .strip_prefix("urn:uuid:")
        .expect("a urn:uuid");
    let groups: Vec<usize> = uuid_text.split('-').map(str::len).collect();
    assert_eq!(groups, [8, 4, 4, 4, 12], "{uuid_text}");
    assert_eq!(&uuid_text[14..15], "8", "version 8: {uuid_text}");
    assert!(
        matches!(&uuid_text[19..20], "8" | "9" | "a" | "b"),
        "variant 10: {uuid_text}"
    );
    let hex = uuid_text.replace('-', "");
    let digest = minted.digest().iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a String");
        hex
    });
    assert_eq!(hex[..12], digest[..12]);
    assert_eq!(hex[13..16], digest[13..16]);
    assert_eq!(hex[17..], digest[17..32]);
}

/// The same draft mints the same `UID`; a draft differing anywhere mints another, even in a
/// field an adapter does not write (a vCard create writes no `BDAY`), and so does the same card
/// in another book. The card's own `id` and `uid` are not part of it.
#[test]
fn every_part_of_the_draft_but_its_id_and_uid_decides_the_uid() {
    let base = content_uid_of(&draft());
    assert_eq!(content_uid_of(&draft()), base);

    let mut renamed_id = draft();
    renamed_id.card.id = ContactId::try_from("another").unwrap();
    renamed_id.card.uid = Some(Uid::new("urn:uuid:set-later").unwrap());
    assert_eq!(content_uid_of(&renamed_id), base);

    let mut birthday = draft();
    birthday.card.anniversaries.insert(
        PropertyId::new("birthday").unwrap(),
        ContactProperty::new(Anniversary {
            kind: Some("birth".into()),
            date: "1815-12-10".into(),
            place: None,
        }),
    );
    assert_ne!(content_uid_of(&birthday), base);

    let mut elsewhere = draft();
    elsewhere.address_book = AddressBookId::try_from("book-2").unwrap();
    assert_ne!(content_uid_of(&elsewhere), base);
}

/// A map's keys are hashed in sorted order, whatever order the map was built in: with
/// serde_json's `preserve_order` feature (which a host's dependency graph may turn on) a map
/// keeps its insertion order, and the same card must still mint the same `UID`. Without the
/// feature both maps already iterate sorted, so the sort itself is tested below.
#[test]
fn the_order_a_map_was_built_in_does_not_change_the_uid() {
    let mut forward = serde_json::Map::new();
    forward.insert("alpha".into(), json!(1));
    forward.insert("omega".into(), json!({"y": 1, "b": 2}));
    let mut backward = serde_json::Map::new();
    backward.insert("omega".into(), json!({"b": 2, "y": 1}));
    backward.insert("alpha".into(), json!(1));

    let with = |extension: serde_json::Map<String, Value>| {
        let mut draft = draft();
        let email = draft.card.emails.values_mut().next().unwrap();
        email.extensions.set("x-acme", Value::Object(extension));
        content_uid_of(&draft)
    };
    assert_eq!(with(forward.clone()), with(backward.clone()));

    let mut canonical = String::new();
    write_canonical(&Value::Object(backward), &mut canonical);
    assert_eq!(canonical, r#"{"alpha":1,"omega":{"b":2,"y":1}}"#);
    let mut plain = String::new();
    write_canonical(&json!(["a", null, true, 1.5, "q\"uote"]), &mut plain);
    assert_eq!(plain, r#"["a",null,true,1.5,"q\"uote"]"#);
}

#[test]
fn the_digest_is_the_sha256_of_the_canonical_draft() {
    let draft = draft();
    let mut value = serde_json::to_value(&draft).unwrap();
    let card = value["card"].as_object_mut().unwrap();
    card.remove("uid");
    card.remove("id");
    let mut canonical = String::new();
    write_canonical(&value, &mut canonical);
    let expected: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
    assert_eq!(content_uid_of(&draft).digest(), &expected);
}

/// The sort itself, fed entries out of order: in this build a `Map` already iterates sorted
/// (no `preserve_order`), so only this shows the canonical form does not depend on it.
#[test]
fn entries_are_written_in_key_order_whatever_order_they_come_in() {
    let (alpha, omega, middle) = ("alpha".to_owned(), "omega".to_owned(), "middle".to_owned());
    let (one, two, three) = (json!(1), json!(2), json!(3));
    let keys: Vec<&str> = sorted(vec![(&omega, &one), (&alpha, &two), (&middle, &three)])
        .into_iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, ["alpha", "middle", "omega"]);
}
