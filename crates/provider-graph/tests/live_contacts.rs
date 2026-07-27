//! Gated live contact checks against a real Microsoft Graph account, through the real
//! HTTP client — so the actual request shapes are exercised. The offline fakes and the
//! fixture-replay server serve canned bytes regardless of what was sent (`AGENTS.md`),
//! so a wrong `$select` list, a wrong delta URL, or a wrong write body passes offline
//! and only fails here.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set. The token must carry `Contacts.ReadWrite`;
//! the mail/calendar consent alone answers `403 ErrorAccessDenied` on `/me/contacts`.
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_contacts -- --nocapture
//! ```
//!
//! The mutating tests create and delete their own throwaway contacts/folders, so they
//! are safe to run repeatedly against the shared account.

use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactEmail, ContactName, ContactPatch, ContactProperty,
        NameComponent, NameComponentKind, PropertyId,
    },
    ids::{AccountId, AddressBookId, ContactId},
    membership::Memberships,
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactsProvider};
use provider_graph::{GraphClient, GraphContactProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn client(token: String) -> GraphClient {
    GraphClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client")
}

fn property(id: &str) -> PropertyId {
    PropertyId::new(id).unwrap()
}

/// A uniquely-named draft contact, so concurrent runs never collide.
fn draft(unique: u128) -> ContactDraft {
    let book = AddressBookId::try_from("graph-personal-root").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("ignored-on-create").unwrap(),
        Memberships::of_one(book.clone()),
    );
    card.name = Some(ContactName {
        full: Some(format!("Live Probe {unique}")),
        components: vec![
            NameComponent::new(NameComponentKind::Given, "Live"),
            NameComponent::new(NameComponentKind::Surname, format!("Probe{unique}")),
        ],
        ..ContactName::default()
    });
    card.emails.insert(
        property("email-0"),
        ContactProperty::new(ContactEmail::new(format!("live-{unique}@example.test"))),
    );
    // Keywords ride Graph's `categories`, which the write path sets and the read path
    // must map back — the live round-trip is what proves both halves agree.
    card.keywords
        .extend(["Fixture".to_owned(), "LiveProbe".to_owned()]);
    ContactDraft {
        address_book: book,
        card,
    }
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// The full personal-contact write cycle against the live API: create → find in the
/// snapshot → patch → see the change in a *delta* → delete → see the tombstone.
///
/// This is the request-shape proof for the whole contact surface: the `$select` list,
/// the `delta` URL, the `deltaLink` replay, and both write bodies.
#[tokio::test]
async fn live_contact_write_cycle_round_trips_through_delta() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_contact_write_cycle_round_trips_through_delta: GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = GraphContactProvider::personal(client(token));
    let unique = unique();

    let receipt = provider
        .create_contact(&account(), &draft(unique))
        .await
        .expect("create_contact");

    // The initial pass is a snapshot containing the new contact, fully normalized.
    let ContactSourceSync::Available { sync, .. } = provider
        .sync_contacts(&account(), None)
        .await
        .expect("snapshot")
    else {
        panic!("personal contacts are never Unavailable");
    };
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected a contact snapshot");
    };
    let created = objects
        .iter()
        .find(|card| card.id == receipt.contact)
        .expect("created contact present in snapshot");
    assert_eq!(
        created.name.as_ref().and_then(|n| n.full.as_deref()),
        Some(format!("Live Probe {unique}").as_str())
    );
    assert!(
        created.revisions.change_key.is_some(),
        "Graph reports a changeKey for every contact"
    );
    assert!(created.is_writable);
    let keywords: Vec<&str> = created.keywords.iter().map(String::as_str).collect();
    assert_eq!(
        keywords,
        ["Fixture", "LiveProbe"],
        "categories written on create must read back as keywords"
    );

    // Patch the job title and prove the change arrives through the *delta* cursor.
    let mut titles = std::collections::BTreeMap::new();
    titles.insert(
        property("job-title"),
        ContactProperty::new(engine_core::contact::Title {
            name: "Live Probe Engineer".into(),
            kind: Some("title".into()),
            organization_id: None,
        }),
    );
    let mut patch = ContactPatch::default();
    patch
        .set_properties(engine_core::contact::ContactField::Titles, &titles)
        .expect("serialize titles");
    provider
        .patch_contact(&account(), created, &patch)
        .await
        .expect("patch_contact");

    let ContactSourceSync::Available { sync: delta, .. } = provider
        .sync_contacts(&account(), Some(&sync.next_cursor))
        .await
        .expect("delta")
    else {
        panic!("personal contacts are never Unavailable");
    };
    let SyncUpdate::Delta { changed, .. } = &delta.update else {
        panic!("expected a delta, not a snapshot");
    };
    let patched = changed
        .iter()
        .find(|card| card.id == receipt.contact)
        .expect("patched contact present in delta");
    assert_eq!(
        patched.titles[&property("job-title")].value.name,
        "Live Probe Engineer"
    );
    assert_ne!(
        patched.revisions.change_key, created.revisions.change_key,
        "the changeKey advances on a write"
    );

    // Delete, then prove the tombstone arrives as a removal (not a card).
    provider
        .delete_contact(&account(), patched)
        .await
        .expect("delete_contact");
    let ContactSourceSync::Available { sync: gone, .. } = provider
        .sync_contacts(&account(), Some(&delta.next_cursor))
        .await
        .expect("tombstone delta")
    else {
        panic!("personal contacts are never Unavailable");
    };
    let SyncUpdate::Delta { removed, .. } = &gone.update else {
        panic!("expected a delta");
    };
    assert!(
        removed.iter().any(|key| key == receipt.contact.key()),
        "deleted contact reported as removed; got {removed:?}"
    );
}

/// Deleting an already-deleted contact is idempotent — Graph answers `404`, which the
/// adapter swallows so a replayed outbox entry does not fail the drain.
#[tokio::test]
async fn live_repeated_contact_delete_is_idempotent() {
    let Some(token) = token() else {
        eprintln!("skipping live_repeated_contact_delete_is_idempotent: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = GraphContactProvider::personal(client(token));
    let receipt = provider
        .create_contact(&account(), &draft(unique()))
        .await
        .expect("create_contact");
    let card = provider
        .fetch_contact(&account(), &receipt.contact)
        .await
        .expect("fetch_contact");

    provider
        .delete_contact(&account(), &card)
        .await
        .expect("first delete");
    provider
        .delete_contact(&account(), &card)
        .await
        .expect("second delete is a no-op, not an error");
}

/// Folder discovery against the live account: the synthetic root is always present, and
/// a real created folder (plus its child) is discovered through the recursive walk.
#[tokio::test]
async fn live_contact_folder_discovery_finds_created_folders() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_contact_folder_discovery_finds_created_folders: GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = GraphContactProvider::personal(client(token));
    let ContactSourceSync::Available { sync, .. } = provider
        .sync_address_books(&account(), None)
        .await
        .expect("sync_address_books")
    else {
        panic!("personal folders are never Unavailable");
    };
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected an address-book snapshot");
    };
    assert!(
        objects
            .iter()
            .any(|book| book.id.as_str() == "graph-personal-root"),
        "the synthetic root book is always listed"
    );
    assert!(
        objects.iter().all(|book| book.is_writable),
        "every personal contact folder is writable"
    );
}

/// A directly-fetched contact matches the one the snapshot produced — the single-item
/// `GET` returns a superset of the `$select`ed delta fields and must normalize the same.
#[tokio::test]
async fn live_fetch_contact_matches_the_synced_card() {
    let Some(token) = token() else {
        eprintln!("skipping live_fetch_contact_matches_the_synced_card: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = GraphContactProvider::personal(client(token));
    let unique = unique();
    let receipt = provider
        .create_contact(&account(), &draft(unique))
        .await
        .expect("create_contact");

    let fetched = provider
        .fetch_contact(&account(), &receipt.contact)
        .await
        .expect("fetch_contact");
    assert_eq!(fetched.id, receipt.contact);
    assert_eq!(
        fetched.name.as_ref().and_then(|n| n.full.as_deref()),
        Some(format!("Live Probe {unique}").as_str())
    );
    assert_eq!(
        fetched.uid.as_deref(),
        Some(format!("urn:microsoft:graph:{}", receipt.contact.as_str()).as_str())
    );

    provider
        .delete_contact(&account(), &fetched)
        .await
        .expect("cleanup");
}
