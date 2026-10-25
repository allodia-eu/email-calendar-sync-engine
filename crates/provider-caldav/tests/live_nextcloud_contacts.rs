//! Gated live integration: CardDAV against the **Nextcloud** harness (`docker/nextcloud/`).
//!
//! A Nextcloud address-book home holds books the account cannot write beside the ones it can:
//! the system address book of the instance's users, and the books apps generate (recently
//! contacted). Stalwart's harness account owns everything it sees, so this is the server that
//! shows the privileges each book reports reach the client. Skips unless `NEXTCLOUD_HTTP_ADDR`
//! is set, so the offline `cargo test --workspace` stays green.

use core::time::Duration;

use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactEmail, ContactField, ContactKind, ContactName,
        ContactPatch, ContactProperty, PropertyId,
    },
    ids::{AccountId, ContactId},
    membership::Memberships,
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactsProvider};
use provider_caldav::{CardDavConfig, CardDavProvider, Credentials};

/// The seeded, writable address book.
const BOOK: &str = "harness";

/// Connects bound to `book`, or `None` to skip (offline gate). Retries briefly so a
/// just-started container is tolerated.
async fn connect(test: &str, book: &str) -> Option<CardDavProvider> {
    let Ok(addr) = std::env::var("NEXTCLOUD_HTTP_ADDR") else {
        eprintln!("skipping {test}: NEXTCLOUD_HTTP_ADDR unset");
        return None;
    };
    let user = std::env::var("NEXTCLOUD_USER").unwrap_or_else(|_| "alice".to_owned());
    let pass = std::env::var("NEXTCLOUD_PASS").unwrap_or_else(|_| "nextcloud-alice-pw".to_owned());
    let config = CardDavConfig::new(
        format!("http://{addr}"),
        Credentials::Basic {
            username: user.clone(),
            password: pass,
        },
        engine_http::test_server::user_agent(),
    )
    .with_address_book(book);
    let mut last_err = None;
    for _ in 0..15 {
        match CardDavProvider::connect(config.clone()).await {
            Ok(provider) => {
                assert_eq!(
                    provider.principal_url(),
                    Some(format!("http://{addr}/remote.php/dav/principals/users/{user}/").as_str())
                );
                return Some(provider);
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect to the Nextcloud harness: {last_err:?}");
}

/// Every book in the home is listed, and only the ones the account may add cards to say so.
#[tokio::test]
async fn nextcloud_lists_each_book_with_its_own_privileges() {
    let Some(provider) = connect("nextcloud_lists_each_book_with_its_own_privileges", BOOK).await
    else {
        return;
    };
    let account = AccountId::try_from("nextcloud-contacts-live").unwrap();

    let ContactSourceSync::Available { sync, .. } = provider
        .sync_address_books(&account, None)
        .await
        .expect("the home is listed")
    else {
        panic!("Nextcloud advertises address books");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("an address-book listing is a snapshot");
    };
    let writable = |suffix: &str| {
        objects
            .iter()
            .find(|book| book.id.as_str().ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} is listed"))
            .is_writable
    };
    assert!(writable("/contacts/"), "the default book");
    assert!(writable("/harness/"), "the seeded book");
    assert!(
        !writable("/z-server-generated--system/"),
        "the instance's users"
    );
    assert!(
        !writable("/z-app-generated--contactsinteraction--recent/"),
        "a book an app fills"
    );
}

/// Bound to a book the account can only read, the provider offers nowhere to create a card.
#[tokio::test]
async fn nextcloud_a_read_only_book_is_no_destination() {
    let Some(provider) = connect(
        "nextcloud_a_read_only_book_is_no_destination",
        "z-server-generated--system",
    )
    .await
    else {
        return;
    };
    assert!(provider.contact_destination().is_none());
    assert!(
        provider
            .rebind(BOOK)
            .expect("rebind")
            .contact_destination()
            .is_some(),
        "the seeded book in the same home is one"
    );
}

/// Create, read back, patch, read back and delete a card in the seeded book.
#[tokio::test]
async fn nextcloud_contact_write_round_trip() {
    let Some(provider) = connect("nextcloud_contact_write_round_trip", BOOK).await else {
        return;
    };
    let account = AccountId::try_from("nextcloud-contacts-live").unwrap();
    let destination = provider
        .contact_destination()
        .expect("writable destination");

    let mut card = ContactCard::new(
        ContactId::try_from("ignored-on-create").unwrap(),
        Memberships::of_one(destination.address_book.clone()),
    );
    // A fresh uid per run: a create is `If-None-Match: *`, so a run that aborted before its
    // cleanup would otherwise leave a card that refuses every later run.
    card.uid = Some(format!(
        "live-nextcloud-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    ));
    card.kind = ContactKind::Individual;
    card.name = Some(ContactName {
        full: Some("Ada Lovelace".into()),
        ..ContactName::default()
    });
    card.emails.insert(
        PropertyId::new("email").unwrap(),
        ContactProperty::new(ContactEmail::new("ada@test.local")),
    );

    let created = provider
        .create_contact(
            &account,
            &ContactDraft {
                address_book: destination.address_book.clone(),
                card,
            },
        )
        .await
        .expect("create");
    let stored = provider
        .fetch_contact(&account, &created.contact)
        .await
        .expect("read the created card back");
    assert_eq!(
        stored.name.as_ref().and_then(|name| name.full.as_deref()),
        Some("Ada Lovelace")
    );

    let mut replacement = std::collections::BTreeMap::new();
    replacement.insert(
        PropertyId::new("email").unwrap(),
        ContactProperty::new(ContactEmail::new("ada@example.invalid")),
    );
    let mut patch = ContactPatch::default();
    patch
        .set_properties(ContactField::Emails, &replacement)
        .unwrap();
    provider
        .patch_contact(&account, &stored, &patch)
        .await
        .expect("patch");
    let patched = provider
        .fetch_contact(&account, &created.contact)
        .await
        .expect("read the patched card back");
    assert_eq!(
        patched
            .emails
            .values()
            .map(|email| email.value.address.clone())
            .collect::<Vec<_>>(),
        vec!["ada@example.invalid".to_owned()]
    );
    assert_eq!(
        patched.name.as_ref().and_then(|name| name.full.as_deref()),
        Some("Ada Lovelace"),
        "the name was not in the patch, so the edit kept it"
    );

    provider
        .delete_contact(&account, &patched)
        .await
        .expect("clean up");
}
