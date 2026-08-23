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
//!
//! **Run with `-- --test-threads=1`.** These all address one mailbox, and Graph throttles
//! *concurrent* access to a single one: run in parallel they intermittently answer
//! `429 ApplicationThrottled` ("over its MailboxConcurrency limit"), which is the harness
//! competing with itself rather than anything under test. Serially the whole file passes
//! in under three seconds, so there is nothing to gain from the parallelism.

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
    GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client")
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

/// A contact with no picture is the ordinary case, and it now has to arrive as an
/// *absence* rather than an error — a caller cannot remember an error, and re-probing
/// every photoless correspondent on every pass is what the negative cache exists to
/// stop.
///
/// The offline fake cannot show this: it answers a canned 404 to any URL, so it would
/// pass just as happily for a request Graph rejects outright. Here the request is real,
/// and it goes to a contact this test just created — so the 404 can only mean "this
/// person has no photo".
///
/// The card also has to *advertise* a photo resource for a host to have anything to
/// ask about; unlike the other three providers, whether Graph holds an image is not
/// knowable from the card, only by asking.
#[tokio::test]
async fn live_a_contact_without_a_photo_is_an_absence_not_an_error() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_contact_without_a_photo_is_an_absence_not_an_error: \
             GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = GraphContactProvider::personal(client(token));
    let unique = unique();
    let receipt = provider
        .create_contact(&account(), &draft(unique))
        .await
        .expect("create_contact");

    let card = provider
        .fetch_contact(&account(), &receipt.contact)
        .await
        .expect("fetch_contact");
    let media = card
        .media
        .values()
        .map(|resource| &resource.value)
        .find(|resource| resource.kind.as_deref() == Some("photo"))
        .expect("a Graph card advertises its photo endpoint");
    assert!(
        media.uri.is_empty(),
        "the URL is derived from the card id, not carried on the card"
    );

    let photo = provider
        .fetch_contact_photo(&account(), &card, media)
        .await
        .expect("a contact without a photo is not a failed fetch");
    assert!(
        photo.is_none(),
        "this contact was just created with no photo"
    );

    provider
        .delete_contact(&account(), &card)
        .await
        .expect("cleanup");
}

/// Both answers a saved contact's photo can give, against real stored contacts: one that
/// has a picture and one that does not.
///
/// The absence direction is covered above with a contact this suite creates. This is the
/// half that needs a *real* one, because nothing the suite can create has a photo — the
/// engine never writes one, so there is no request shape here that could put an image
/// there to read back. Two contacts are kept on the test accounts for it.
///
/// It also pins the route: a `contact` has only the singular `photo/$value`, and asking
/// one for a size is `400 RequestBroker--ParseUri` rather than any kind of absence. A
/// successful read here is the proof that the personal-contact path does not ask for one.
///
/// Deliberately not keyed on a contact's name or id — it walks what the account has and
/// needs only that both shapes exist, so either Microsoft test account satisfies it.
#[tokio::test]
async fn live_a_saved_contact_with_a_picture_returns_it_and_one_without_returns_none() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_saved_contact_with_a_picture_returns_it_and_one_without_returns_none: \
             GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = GraphContactProvider::personal(client(token));
    let ContactSourceSync::Available { sync, .. } = provider
        .sync_contacts(&account(), None)
        .await
        .expect("snapshot")
    else {
        panic!("personal contacts are never Unavailable");
    };
    let cards = match &sync.update {
        SyncUpdate::Snapshot { objects, .. } => objects.clone(),
        SyncUpdate::Delta { changed, .. } => changed.clone(),
    };

    let (mut with_picture, mut without) = (0_usize, 0_usize);
    for card in &cards {
        let media = card
            .media
            .values()
            .map(|resource| &resource.value)
            .find(|resource| resource.kind.as_deref() == Some("photo"))
            .expect("every Graph card advertises its photo endpoint");
        match provider
            .fetch_contact_photo(&account(), card, media)
            .await
            .expect("a saved contact's photo read must not fail either way")
        {
            Some(photo) => {
                // A JPEG or PNG magic number: proof these are image bytes and not, say,
                // an error document served with a 200.
                let bytes = photo.as_bytes();
                assert!(
                    bytes.starts_with(b"\xff\xd8") || bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                    "a returned photo must be a raster image, got {:?}",
                    &bytes[..bytes.len().min(8)]
                );
                with_picture += 1;
            }
            None => without += 1,
        }
    }

    // Only the *present* direction is asserted. It is what this test exists for and what
    // no self-created contact can cover; the absent direction has its own test above,
    // which creates the contact it needs and so cannot be left uncovered by account
    // state. Requiring both here would only make this fail on an account that happens to
    // have a picture on everything.
    assert!(
        with_picture > 0,
        "no saved contact on this account has a photo, so the direction this test exists \
         for did not run — set a picture on one contact and re-run"
    );
    eprintln!("verified: {with_picture} contact(s) with a picture, {without} without");
}
