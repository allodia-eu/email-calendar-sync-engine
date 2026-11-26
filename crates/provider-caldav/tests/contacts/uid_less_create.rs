//! A card created with no `UID`, against both harness servers with CardDAV (Stalwart,
//! Nextcloud).
//!
//! A card read from Google People or Microsoft Graph carries no `UID`, and RFC 6352 §5.1 requires
//! one in every address object, so a create mints one (`urn:uuid:`, an RFC 9562 UUIDv8 of the
//! bytes it sends) and names the resource by the same hash. What only a real server answers:
//! that it keeps the minted `UID` rather than putting its own in place (Nextcloud gives a card
//! sent without one a `UID` of its own: observed on import), that it lists the card under the
//! name it was written to, and that the same draft sent again meets that card.

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactEmail, ContactName, ContactProperty, PropertyId},
    error::FailureClass,
    ids::{AccountId, ContactId},
    membership::Memberships,
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactsProvider};
use provider_caldav::CardDavProvider;

/// Creates a card with no `UID`, reads it back by the id the write answered and through the
/// book's listing, sends the same draft again, and deletes the card.
pub(crate) async fn uid_less_create_round_trip(provider: &CardDavProvider, account: &AccountId) {
    let destination = provider
        .contact_destination()
        .expect("writable destination");
    // A fresh card per run: the name is the hash of what is sent, so a run that aborted before
    // its cleanup would otherwise leave a card every later run meets.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let mut card = ContactCard::new(
        ContactId::try_from("ignored-on-create").unwrap(),
        Memberships::of_one(destination.address_book.clone()),
    );
    assert_eq!(card.uid, None);
    card.name = Some(ContactName {
        full: Some(format!("No Uid {nonce}")),
        ..ContactName::default()
    });
    card.emails.insert(
        PropertyId::new("email").unwrap(),
        ContactProperty::new(ContactEmail::new(format!("no-uid-{nonce}@test.local"))),
    );
    let draft = ContactDraft {
        address_book: destination.address_book.clone(),
        card,
    };

    let created = provider
        .create_contact(account, &draft)
        .await
        .expect("a card without a UID is created");
    assert!(
        created.contact.as_str().contains("/sha256-"),
        "named by the hash of what was sent: {}",
        created.contact.as_str()
    );
    let stored = provider
        .fetch_contact(account, &created.contact)
        .await
        .expect("read the created card back");
    let uid = stored.uid.clone().expect("the stored card has a UID");
    let minted = uid
        .as_str()
        .strip_prefix("urn:uuid:")
        .unwrap_or_else(|| panic!("the server kept the minted UID, not its own: {uid}"));
    assert_eq!(&minted[14..15], "8", "the minted UUIDv8: {uid}");

    let ContactSourceSync::Available { sync, .. } = provider
        .sync_contacts(account, None)
        .await
        .expect("the book is listed")
    else {
        panic!("the destination book is available");
    };
    let listed: Vec<ContactCard> = match sync.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    };
    let listed = listed
        .iter()
        .find(|card| card.uid.as_ref() == Some(&uid))
        .expect("the listing holds the card");
    assert_eq!(
        listed.id, created.contact,
        "listed under the id the write answered"
    );

    let again = provider
        .create_contact(account, &draft)
        .await
        .expect_err("the same draft meets the card it already wrote");
    assert_eq!(again.class(), FailureClass::Conflict, "{again}");

    provider
        .delete_contact(account, &stored)
        .await
        .expect("clean up");
}
