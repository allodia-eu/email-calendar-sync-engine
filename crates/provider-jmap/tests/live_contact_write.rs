//! Gated live `ContactCard/set` checks against the Stalwart harness. Skips with no
//! `STALWART_HTTP_ADDR`.
//!
//! The adapter reads an update or a destroy as done only when the server's answer names the
//! card in `updated` or `destroyed` (RFC 8620 §5.3), and as unconfirmed when it names it
//! nowhere. That is only safe if a real server does name it, which the offline suite cannot
//! show: it serves whatever answer it was given. So this drives create, patch and delete
//! through the adapter and checks each landed, then deletes again to see that an already-gone
//! card still reads as done. The draft has no `uid`, so the read back also shows the one the
//! adapter minted was kept.

mod common;

use common::*;
use engine_core::contact::{
    ContactCard, ContactDraft, ContactField, ContactName, ContactPatch, FieldPatch,
};
use engine_provider::{ContactSourceSync, ContactsProvider};

#[tokio::test]
async fn a_card_is_created_patched_and_deleted_and_each_answer_says_so() {
    let Some(provider) = setup("contact_write").await else {
        return;
    };
    let books = match provider
        .sync_address_books(&account(), None)
        .await
        .expect("address books")
    {
        ContactSourceSync::Available { sync, .. } => sync.update.changed().to_vec(),
        ContactSourceSync::Unavailable(reason) => panic!("no address books: {reason:?}"),
    };
    let book = books
        .iter()
        .find(|book| book.is_writable)
        .expect("a writable address book")
        .id
        .clone();
    let provider = provider.with_contact_address_book(book.clone());

    let mut card = ContactCard::new(
        engine_core::ids::ContactId::try_from("ignored").unwrap(),
        engine_core::membership::Memberships::of_one(book.clone()),
    );
    card.name = Some(ContactName {
        full: Some("Live Write".into()),
        ..ContactName::default()
    });
    let created = provider
        .create_contact(
            &account(),
            &ContactDraft {
                address_book: book,
                card,
            },
        )
        .await
        .expect("create");

    let stored = provider
        .fetch_contact(&account(), &created.contact)
        .await
        .expect("the created card");
    // The draft has no `uid` (as a card read from Google People or Microsoft Graph has none), and
    // RFC 9553 §2.1.9 makes it mandatory, so the adapter minted one (`content_uid_of`, a UUIDv8)
    // and the server kept it.
    let uid = stored.uid.as_ref().expect("the created card has a uid");
    let minted = uid
        .as_str()
        .strip_prefix("urn:uuid:")
        .expect("the uid the adapter minted");
    assert_eq!(&minted[14..15], "8", "a UUIDv8");
    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(serde_json::json!({ "full": "Live Write Renamed" })),
    );
    provider
        .patch_contact(&account(), &stored, &patch)
        .await
        .expect("the server names the card in `updated`");
    let renamed = provider
        .fetch_contact(&account(), &created.contact)
        .await
        .expect("the patched card");
    assert_eq!(
        renamed.display_name().as_deref(),
        Some("Live Write Renamed")
    );

    provider
        .delete_contact(&account(), &renamed)
        .await
        .expect("the server names the card in `destroyed`");
    provider
        .delete_contact(&account(), &renamed)
        .await
        .expect("an already-gone card is done, not unconfirmed");
}
