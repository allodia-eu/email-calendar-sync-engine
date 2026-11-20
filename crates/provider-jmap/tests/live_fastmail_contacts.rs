//! Gated live `ContactCard/set` against Fastmail. Skips with no `FASTMAIL_API_TOKEN` (an API
//! token: Fastmail's JMAP takes no app password).
//!
//! Fastmail validates a card more strictly than the Stalwart harness: it refuses a name property
//! sent as `null`, and it reorders a name's components unless the card declares them ordered.
//! Neither shows against Stalwart, so this writes a card the way a host's form does (a given name
//! and a surname, nothing else), reads it back, and deletes it.

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactName, NameComponent, NameComponentKind},
    ids::{AccountId, ContactId},
    membership::Memberships,
};
use engine_provider::{ContactSourceSync, ContactsProvider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};

#[tokio::test]
async fn a_card_named_by_its_components_is_stored_in_reading_order() {
    let Some(token) = std::env::var("FASTMAIL_API_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
    else {
        eprintln!("FASTMAIL_API_TOKEN unset; skipping");
        return;
    };
    let account = AccountId::try_from("live").unwrap();
    let provider = JmapProvider::connect(JmapConfig::new(
        "https://api.fastmail.com/jmap/session".to_owned(),
        Credentials::bearer(token),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect");
    let ContactSourceSync::Available { sync, .. } = provider
        .sync_address_books(&account, None)
        .await
        .expect("address books")
    else {
        panic!("Fastmail lists no address books");
    };
    let book = sync
        .update
        .changed()
        .iter()
        .find(|book| book.is_writable)
        .expect("a writable address book")
        .id
        .clone();
    let provider = provider.with_contact_address_book(book.clone());

    let mut card = ContactCard::new(
        ContactId::try_from("ignored").unwrap(),
        Memberships::of_one(book.clone()),
    );
    card.name = Some(ContactName {
        components: vec![
            NameComponent::new(NameComponentKind::Given, "Live"),
            NameComponent::new(NameComponentKind::Surname, "Componentcard"),
        ],
        ..ContactName::default()
    });
    let created = provider
        .create_contact(
            &account,
            &ContactDraft {
                address_book: book,
                card,
            },
        )
        .await
        .expect("Fastmail stores a card with only name components");
    let stored = provider
        .fetch_contact(&account, &created.contact)
        .await
        .expect("the created card");
    provider
        .delete_contact(&account, &stored)
        .await
        .expect("delete");
    assert_eq!(stored.display_name().as_deref(), Some("Live Componentcard"));
}
