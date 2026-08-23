//! Gated live contact checks against a real Google account, through the real HTTP
//! client — so the actual request shapes and, critically, the **API host** are
//! exercised.
//!
//! This file is the only thing that can prove the People host: People is served from
//! `people.googleapis.com`, not the `www.googleapis.com` root that Gmail and Calendar
//! share, and every offline test drives a custom base (a replay server or the routing
//! fake), so a contact URL resolves to the test origin no matter which host the provider
//! named. A wrong host passes the whole offline suite and 404s in production.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set. The token must carry the contacts scopes,
//! and the **People API** must be enabled on the Cloud project (a disabled project
//! answers `403 SERVICE_DISABLED`; note the console lists a separate, legacy "Contacts
//! API" that is *not* this one).
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_contacts -- --nocapture
//! ```
//!
//! The mutating tests create and delete their own throwaway contacts. Google's sync
//! tokens propagate asynchronously (a change can take ~10s to appear in a delta), so the
//! delta assertions poll rather than read once.

use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactEmail, ContactName, ContactProperty, NameComponent,
        NameComponentKind, PropertyId,
    },
    ids::{AccountId, AddressBookId, ContactId},
    membership::Memberships,
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ContactSourceSync, ContactsProvider};
use provider_google::{GoogleClient, GoogleContactProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn client(token: String) -> GoogleClient {
    GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client")
}

fn property(id: &str) -> PropertyId {
    PropertyId::new(id).unwrap()
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn draft(unique: u128) -> ContactDraft {
    let book = AddressBookId::try_from("google-connections").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("people/ignored-on-create").unwrap(),
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
    ContactDraft {
        address_book: book,
        card,
    }
}

/// Polls `sync_contacts` from `cursor` until `found` accepts a result, or gives up.
///
/// Google's People sync tokens are eventually consistent: a write is visible to a
/// *direct* read immediately but can take several seconds to surface in a delta, so a
/// single read right after a mutation reliably returns an empty page.
async fn poll_delta<F>(
    provider: &GoogleContactProvider,
    cursor: &SyncState,
    found: F,
) -> SyncUpdate<ContactCard>
where
    F: Fn(&SyncUpdate<ContactCard>) -> bool,
{
    for attempt in 0..12 {
        let ContactSourceSync::Available { sync, .. } = provider
            .sync_contacts(&account(), Some(cursor))
            .await
            .expect("delta")
        else {
            panic!("owned connections are never Unavailable");
        };
        if found(&sync.update) {
            return sync.update;
        }
        eprintln!("delta not yet propagated (attempt {})", attempt + 1);
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    panic!("change never surfaced in a delta");
}

/// The full owned-contact write cycle against the live API: create → snapshot → patch →
/// delta → delete → tombstone.
///
/// Reaching a `200` at all is the proof that contact calls are rooted at
/// `people.googleapis.com`; the `www.googleapis.com` root answers an HTML `404` for
/// every one of these paths.
#[tokio::test]
async fn live_contact_write_cycle_round_trips_through_delta() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_contact_write_cycle_round_trips_through_delta: GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = GoogleContactProvider::connections(client(token));
    let unique = unique();

    let receipt = provider
        .create_contact(&account(), &draft(unique))
        .await
        .expect("create_contact");

    let ContactSourceSync::Available { sync, .. } = provider
        .sync_contacts(&account(), None)
        .await
        .expect("snapshot")
    else {
        panic!("owned connections are never Unavailable");
    };
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected a contact snapshot");
    };
    let created = objects
        .iter()
        .find(|card| card.id == receipt.contact)
        .expect("created contact present in snapshot");
    // People **derives** `displayName` from the name components and ignores any
    // supplied full name, so the round-tripped value is `givenName + " " + familyName`
    // — not the `full` the draft carried.
    assert_eq!(
        created.name.as_ref().and_then(|n| n.full.as_deref()),
        Some(format!("Live Probe{unique}").as_str()),
        "displayName is server-derived from the components"
    );
    assert!(
        created.revisions.etag.is_some(),
        "People returns an etag for every person; it is the write precondition"
    );
    assert!(created.is_writable);

    // Patch the organization title and wait for it to reach a delta.
    let mut organizations = std::collections::BTreeMap::new();
    organizations.insert(
        property("organization-0"),
        ContactProperty::new(engine_core::contact::Organization {
            name: "Analytical Engines BV".into(),
            ..engine_core::contact::Organization::default()
        }),
    );
    let mut patch = engine_core::contact::ContactPatch::default();
    patch
        .set_properties(
            engine_core::contact::ContactField::Organizations,
            &organizations,
        )
        .expect("serialize organizations");
    provider
        .patch_contact(&account(), created, &patch)
        .await
        .expect("patch_contact");

    let id = receipt.contact.clone();
    let update = poll_delta(&provider, &sync.next_cursor, |update| {
        matches!(update, SyncUpdate::Delta { changed, .. }
            if changed.iter().any(|card| card.id == id))
    })
    .await;
    let SyncUpdate::Delta { changed, .. } = &update else {
        panic!("expected a delta, not a snapshot");
    };
    let patched = changed
        .iter()
        .find(|card| card.id == receipt.contact)
        .expect("patched contact in delta");
    assert!(
        patched
            .organizations
            .values()
            .any(|org| org.value.name == "Analytical Engines BV"),
        "the patched organization arrives through the delta"
    );

    // Delete, then wait for the tombstone.
    provider
        .delete_contact(&account(), patched)
        .await
        .expect("delete_contact");
    let id = receipt.contact.clone();
    poll_delta(&provider, &sync.next_cursor, |update| {
        matches!(update, SyncUpdate::Delta { removed, .. }
            if removed.iter().any(|key| key == id.key()))
    })
    .await;
}

/// A quiet incremental sync — no changes since the cursor — is the steady state, and
/// People answers it with a bare `{"nextSyncToken": …}`. It must advance the cursor
/// rather than fail as a malformed page.
#[tokio::test]
async fn live_empty_delta_advances_the_cursor() {
    let Some(token) = token() else {
        eprintln!("skipping live_empty_delta_advances_the_cursor: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = GoogleContactProvider::connections(client(token));
    let ContactSourceSync::Available { sync, .. } = provider
        .sync_contacts(&account(), None)
        .await
        .expect("snapshot")
    else {
        panic!("owned connections are never Unavailable");
    };
    // Immediately replaying the fresh token: nothing has changed in between.
    let ContactSourceSync::Available { sync: delta, .. } = provider
        .sync_contacts(&account(), Some(&sync.next_cursor))
        .await
        .expect("an empty delta is a valid response, not a protocol error")
    else {
        panic!("owned connections are never Unavailable");
    };
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &delta.update
    else {
        panic!("expected a delta");
    };
    assert!(changed.is_empty() && removed.is_empty());
    assert!(!delta.next_cursor.as_str().is_empty(), "cursor advanced");
}

/// Other Contacts and contact groups are separate live sources with their own cursors —
/// both reachable on a consumer account.
#[tokio::test]
async fn live_other_contacts_and_groups_sync_independently() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_other_contacts_and_groups_sync_independently: GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };
    let ContactSourceSync::Available { sync, .. } =
        GoogleContactProvider::other_contacts(client(token.clone()))
            .sync_contacts(&account(), None)
            .await
            .expect("other contacts")
    else {
        panic!("Other Contacts is available on a consumer account");
    };
    assert!(matches!(sync.update, SyncUpdate::Snapshot { .. }));

    let ContactSourceSync::Available { sync: groups, .. } =
        GoogleContactProvider::groups(client(token))
            .sync_contacts(&account(), None)
            .await
            .expect("contact groups")
    else {
        panic!("contact groups are available on a consumer account");
    };
    let SyncUpdate::Snapshot { objects, .. } = &groups.update else {
        panic!("groups are always a snapshot");
    };
    assert!(
        !objects.is_empty(),
        "an account always has the system groups"
    );
    assert_eq!(groups.next_cursor.as_str(), "google-groups-snapshot");
}

/// **A consumer account has no Workspace directory**, and People refuses it with
/// `400 FAILED_PRECONDITION` ("Must be a G Suite domain user") rather than `403`. The
/// directory is optional, so the source must degrade instead of failing the sync.
#[tokio::test]
async fn live_directory_source_degrades_on_a_consumer_account() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_directory_source_degrades_on_a_consumer_account: GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };
    let result = GoogleContactProvider::directory(client(token))
        .sync_contacts(&account(), None)
        .await
        .expect("an optional source refusal is not a sync failure");
    assert!(
        matches!(result, ContactSourceSync::Unavailable(_)),
        "consumer accounts have no directory; got {result:?}"
    );
}

/// A stale etag on `updateContact` is a real `400 FAILED_PRECONDITION`, which must
/// classify as a refetch-and-retry `Conflict` — not a permanent failure that would make
/// a host drop the write.
#[tokio::test]
async fn live_stale_etag_update_is_a_conflict() {
    let Some(token) = token() else {
        eprintln!("skipping live_stale_etag_update_is_a_conflict: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = GoogleContactProvider::connections(client(token));
    let receipt = provider
        .create_contact(&account(), &draft(unique()))
        .await
        .expect("create_contact");
    let stale = provider
        .fetch_contact(&account(), &receipt.contact)
        .await
        .expect("fetch_contact");

    // First patch advances the server-side etag, making `stale` out of date.
    let mut organizations = std::collections::BTreeMap::new();
    organizations.insert(
        property("organization-0"),
        ContactProperty::new(engine_core::contact::Organization {
            name: "First Write".into(),
            ..engine_core::contact::Organization::default()
        }),
    );
    let mut patch = engine_core::contact::ContactPatch::default();
    patch
        .set_properties(
            engine_core::contact::ContactField::Organizations,
            &organizations,
        )
        .expect("serialize organizations");
    provider
        .patch_contact(&account(), &stale, &patch)
        .await
        .expect("first patch");

    // Replaying the same base card now carries the superseded etag.
    let error = provider
        .patch_contact(&account(), &stale, &patch)
        .await
        .expect_err("a stale etag must be refused");
    assert_eq!(
        error.class(),
        engine_core::error::FailureClass::Conflict,
        "a stale-etag write is recoverable by refetch, not permanent: {error:?}"
    );

    let current = provider
        .fetch_contact(&account(), &receipt.contact)
        .await
        .expect("refetch");
    provider
        .delete_contact(&account(), &current)
        .await
        .expect("cleanup");
}

/// Decodes a JPEG/PNG's pixel dimensions from its header.
///
/// The assertion below is about *what size image came back*, and nothing weaker
/// distinguishes a size request that worked from one the CDN ignored — both are 200
/// with a valid picture.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        return Some((width, height));
    }
    if !bytes.starts_with(b"\xff\xd8") {
        return None;
    }
    let mut index = 2;
    while index + 9 < bytes.len() {
        if bytes[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        if (0xC0..=0xC2).contains(&marker) {
            let height = u32::from(u16::from_be_bytes(
                bytes[index + 5..index + 7].try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes[index + 7..index + 9].try_into().ok()?,
            ));
            return Some((width, height));
        }
        index += 2 + usize::from(u16::from_be_bytes(
            bytes[index + 2..index + 4].try_into().ok()?,
        ));
    }
    None
}

/// Google's photo CDN takes the size as an **option suffix on the path** (`…=s240`),
/// and accepts `?sz=240` while silently ignoring it — 200, a valid image, the original
/// pixels. Nothing offline can tell those apart: a fake serves canned bytes whatever
/// the URL, and both variants are a successful fetch of a real picture.
///
/// So this asserts the pixels. A contact photo arrives from `photos[].url` already
/// carrying a suffix (`=s100`), which is why appending rather than replacing is not
/// enough either.
#[tokio::test]
async fn live_a_contact_photo_arrives_at_the_size_we_asked_for() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_contact_photo_arrives_at_the_size_we_asked_for: \
             GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = GoogleContactProvider::connections(client(token));
    // People meters *full* syncs per account, and this file's other tests each spend
    // one. Exhausting that quota is a property of the shared throwaway account, not a
    // defect, so it aborts loudly rather than failing — but it must never read as a
    // pass, because the assertion below did not run.
    let sync = match provider.sync_contacts(&account(), None).await {
        Ok(ContactSourceSync::Available { sync, .. }) => sync,
        Ok(ContactSourceSync::Unavailable(reason)) => {
            panic!("connections are available on a consumer account: {reason:?}")
        }
        Err(error) if error.class() == engine_core::error::FailureClass::RateLimited => {
            eprintln!(
                "!! NOT VERIFIED: People sync quota exhausted, so the photo-size \
                 assertion did not run. Wait for the quota to reset and re-run."
            );
            return;
        }
        Err(error) => panic!("snapshot: {error:?}"),
    };
    let cards = match &sync.update {
        SyncUpdate::Snapshot { objects, .. } => objects.clone(),
        SyncUpdate::Delta { changed, .. } => changed.clone(),
    };
    // `photos[].url` is only present for a person who has one; the normalizer drops
    // Google's generated monogram placeholders (`default: true`).
    let Some((card, media)) = cards.iter().find_map(|card| {
        card.media
            .values()
            .map(|resource| &resource.value)
            .find(|resource| resource.kind.as_deref() == Some("photo"))
            .map(|resource| (card, resource.clone()))
    }) else {
        eprintln!("skipping: no contact on this account has a photo");
        return;
    };

    let photo = provider
        .fetch_contact_photo(&account(), card, &media)
        .await
        .expect("the photo fetch succeeds")
        .expect("a card advertising a photo has one");
    assert_eq!(
        dimensions(photo.as_bytes()),
        Some((240, 240)),
        "the CDN must return the avatar size we asked for, not the stored original"
    );
    // People stamps every person with an `etag`, so that is what validates the cached
    // bytes; the URL is only the fallback for a source that versions nothing.
    assert_eq!(
        Some(photo.fingerprint.as_str()),
        media.fingerprint.as_deref(),
        "the card's own revision keys the cache, not the sized URL fetched"
    );
}
