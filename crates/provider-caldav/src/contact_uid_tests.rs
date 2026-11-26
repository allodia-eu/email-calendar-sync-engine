//! A card's `UID` over CardDAV: what a read takes as one, and the one a create sends.

use std::sync::Arc;

use engine_core::{
    contact::{
        Anniversary, ContactCard, ContactDraft, ContactName, ContactPhone, ContactProperty,
        PropertyId,
    },
    ids::{AddressBookId, ContactId, Uid},
    membership::Memberships,
};
use engine_provider::{ContactsProvider, IgnoreConnectSteps};

use crate::{
    CardDavProvider,
    carddav_tests::{BOOKS, PRINCIPAL, account},
    test_support::{Replay, ok, options, wrote},
    vcard::parse_vcard,
};

const BOOK: &str = "/dav/addressbooks/alice/default/";

fn read(uid_line: &str) -> Option<Uid> {
    let raw = format!("BEGIN:VCARD\r\nVERSION:4.0\r\n{uid_line}FN:Ada\r\nEND:VCARD\r\n");
    let card = parse_vcard(
        &raw,
        ContactId::try_from("/book/ada.vcf").unwrap(),
        AddressBookId::try_from(BOOK).unwrap(),
        true,
    )
    .expect("a card with a bad UID is still a card");
    assert_eq!(card.display_name().as_deref(), Some("Ada"));
    card.uid
}

#[test]
fn a_uid_no_uid_type_takes_reads_as_none_and_keeps_the_card() {
    assert_eq!(
        read("UID:urn:uuid:ada\r\n"),
        Some(Uid::new("urn:uuid:ada").unwrap())
    );
    assert_eq!(read(""), None);
    assert_eq!(read("UID:\r\n"), None);
    let long = "u".repeat(Uid::MAX_OCTETS + 1);
    assert_eq!(read(&format!("UID:{long}\r\n")), None);
}

/// One rule decides a card's `UID` for a sync and an import (`vcard::card_uid`): the first `UID`
/// property, whatever its group, its value after the first `:` outside a quoted parameter
/// (RFC 6350 §3.3), unescaped and trimmed.
#[test]
fn a_cards_uid_is_read_one_way() {
    assert_eq!(
        read("UID;X-NOTE=\"a:b\":urn:uuid:quoted\r\n"),
        Some(Uid::new("urn:uuid:quoted").unwrap())
    );
    assert_eq!(
        read("item1.uid: urn:uuid:grouped \r\n"),
        Some(Uid::new("urn:uuid:grouped").unwrap())
    );
    assert_eq!(read("UID:a\\,b\r\n"), Some(Uid::new("a,b").unwrap()));
    assert_eq!(
        read("UID:urn:uuid:first\r\nUID:urn:uuid:second\r\n"),
        Some(Uid::new("urn:uuid:first").unwrap())
    );
}

fn named(phone: Option<&str>) -> ContactCard {
    let mut card = ContactCard::new(
        ContactId::try_from("ignored").unwrap(),
        Memberships::of_one(AddressBookId::try_from(BOOK).unwrap()),
    );
    card.name = Some(ContactName {
        full: Some("Ada Lovelace".into()),
        ..ContactName::default()
    });
    if let Some(number) = phone {
        card.phones.insert(
            PropertyId::new("phone").unwrap(),
            ContactProperty::new(ContactPhone {
                number: number.into(),
                ..ContactPhone::default()
            }),
        );
    }
    card
}

/// Creates each card in turn and answers each create's `PUT` as stored: `(href, body)` per card.
async fn created(cards: Vec<ContactCard>) -> Vec<(String, String)> {
    let mut answers = vec![ok(PRINCIPAL), options(Some("1, 3, addressbook")), ok(BOOKS)];
    answers.extend(cards.iter().map(|_| wrote(201, Some("\"v1\""))));
    let replay = Arc::new(Replay::new(answers));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();
    for card in cards {
        provider
            .create_contact(
                &account(),
                &ContactDraft {
                    address_book: AddressBookId::try_from(BOOK).unwrap(),
                    card,
                },
            )
            .await
            .unwrap();
    }
    replay
        .writes()
        .iter()
        .map(|write| (write.href.clone(), write.body.clone()))
        .collect()
}

fn hex(digest: &[u8]) -> String {
    digest.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a String");
        hex
    })
}

fn with_birthday(mut card: ContactCard) -> ContactCard {
    card.anniversaries.insert(
        PropertyId::new("birthday").unwrap(),
        ContactProperty::new(Anniversary {
            date: "1815-12-10".into(),
            kind: Some("birth".into()),
            place: None,
        }),
    );
    card
}

/// RFC 6352 §5.1: every address object carries a `UID`. A card from a source without a real one
/// has none, so a create mints one from the whole draft (`engine_provider::content_uid_of`) and
/// names the resource by the same hash: the same draft mints the same `UID` and name, so a
/// retried create meets its own earlier write, and a card differing anywhere mints others, even
/// in a field the vCard it sends leaves out (a birthday), so a second card is never mistaken for
/// a retry of the first.
#[tokio::test]
async fn a_card_without_a_uid_is_created_with_one_minted_from_the_whole_draft() {
    let writes = created(vec![
        named(Some("+1 555 0100")),
        named(Some("+1 555 0100")),
        with_birthday(named(Some("+1 555 0100"))),
    ])
    .await;
    let (href, body) = &writes[0];

    let minted = engine_provider::content_uid_of(&ContactDraft {
        address_book: AddressBookId::try_from(BOOK).unwrap(),
        card: named(Some("+1 555 0100")),
    });
    let uids: Vec<&str> = body
        .split("\r\n")
        .filter_map(|line| line.strip_prefix("UID:"))
        .collect();
    assert_eq!(uids, [minted.uid().as_str()], "{body}");
    assert_eq!(
        href,
        &format!("{BOOK}sha256-{}.vcf", hex(minted.digest())),
        "named by the hash the UID was minted from"
    );

    // What the server stores reads back as that `UID`.
    let stored = parse_vcard(
        body,
        ContactId::try_from(href.as_str()).unwrap(),
        AddressBookId::try_from(BOOK).unwrap(),
        true,
    )
    .unwrap();
    assert_eq!(stored.uid.as_ref(), Some(minted.uid()));

    assert_eq!(
        writes[1], writes[0],
        "the same draft mints the same UID and name"
    );
    assert!(
        !writes[2].1.contains("BDAY"),
        "the vCard a create sends leaves the birthday out"
    );
    assert_ne!(
        writes[2].0, writes[0].0,
        "yet another card is another resource"
    );
    assert!(!writes[2].1.contains(uids[0]), "and another UID");
}

#[tokio::test]
async fn a_card_with_a_uid_is_sent_and_named_by_it() {
    let mut card = named(None);
    card.uid = Some(Uid::new("ada@example.test").unwrap());
    let writes = created(vec![card]).await;
    assert_eq!(writes[0].0, format!("{BOOK}ada@example.test.vcf"));
    let uids: Vec<&str> = writes[0]
        .1
        .split("\r\n")
        .filter_map(|line| line.strip_prefix("UID:"))
        .collect();
    assert_eq!(uids, ["ada@example.test"]);
}
