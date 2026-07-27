//! Contact sync driven against the **captured** Google People responses in
//! `tests/fixtures/contacts/`, rather than hand-written JSON.
//!
//! The hand-written cases in `contact_tests.rs` prove the orchestration branches; these
//! prove the adapter against shapes the live People API actually returned — notably the
//! *empty* incremental delta, which carries `nextSyncToken` and **no** `connections` key
//! at all. See `tests/fixtures/README.md` for how they were captured.

use engine_core::{
    ids::AccountId,
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ContactSourceSync, ContactsProvider};

use crate::{
    GoogleContactProvider,
    test_support::{fake_client, fake_client_fallible, json},
};

const CONNECTIONS: &str = include_str!("../tests/fixtures/contacts/connections.json");
const DELTA: &str = include_str!("../tests/fixtures/contacts/connections_delta.json");
const DELTA_NOCHANGE: &str =
    include_str!("../tests/fixtures/contacts/connections_delta_nochange.json");
const DELTA_REMOVED: &str =
    include_str!("../tests/fixtures/contacts/connections_delta_removed.json");
const PERSON: &str = include_str!("../tests/fixtures/contacts/person.json");
const OTHER_CONTACTS: &str = include_str!("../tests/fixtures/contacts/other_contacts.json");
const CONTACT_GROUPS: &str = include_str!("../tests/fixtures/contacts/contact_groups.json");
const DIRECTORY_PRECONDITION: &str =
    include_str!("../tests/fixtures/error/contacts_directory_precondition.json");
const STALE_ETAG: &str = include_str!("../tests/fixtures/error/contacts_stale_etag.json");
const SYNC_TOKEN_INVALID: &str =
    include_str!("../tests/fixtures/error/contacts_sync_token_invalid.json");

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

/// **An incremental People delta with nothing to report omits the collection key
/// entirely** — the captured body is exactly `{"nextSyncToken": "…"}`. This is the
/// steady state of every quiet sync, so it must produce an empty delta and advance the
/// cursor, not fail as a malformed page.
#[tokio::test]
async fn captured_empty_delta_advances_the_cursor_instead_of_failing() {
    let client = fake_client(vec![("/v1/people/me/connections", json(DELTA_NOCHANGE))]);
    let cursor = SyncState::new("google-sync-token-2");
    let ContactSourceSync::Available { sync, .. } = GoogleContactProvider::connections(client)
        .sync_contacts(&account(), Some(&cursor))
        .await
        .expect("an empty delta is a valid response, not a protocol error")
    else {
        panic!("expected available");
    };
    let SyncUpdate::Delta { changed, removed } = sync.update else {
        panic!("expected a delta");
    };
    assert!(changed.is_empty());
    assert!(removed.is_empty());
    assert_eq!(sync.next_cursor.as_str(), "google-sync-token-3");
}

/// A page carrying neither the collection nor a sync token is still malformed and must
/// not advance a cursor — the guard the empty-delta fix must not weaken.
#[tokio::test]
async fn a_page_without_a_sync_token_is_still_malformed() {
    for page in [
        serde_json::json!({"connections": []}),
        serde_json::json!({}),
    ] {
        let client = fake_client(vec![("/v1/people/me/connections", page)]);
        assert!(
            GoogleContactProvider::connections(client)
                .sync_contacts(&account(), None)
                .await
                .is_err(),
            "a page with no nextSyncToken must fail"
        );
    }
}

/// The captured snapshot normalizes the field-complete seeded person.
#[tokio::test]
async fn captured_connections_snapshot_normalizes_the_person() {
    let client = fake_client(vec![("/v1/people/me/connections", json(CONNECTIONS))]);
    let ContactSourceSync::Available { sync, .. } = GoogleContactProvider::connections(client)
        .sync_contacts(&account(), None)
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let card = objects
        .iter()
        .find(|card| {
            card.name
                .as_ref()
                .and_then(|name| name.full.as_deref())
                .is_some_and(|full| full.contains("Lovelace"))
        })
        .expect("the seeded contact");
    assert!(card.emails.len() >= 2);
    assert!(card.phones.len() >= 2);
    assert!(!card.addresses.is_empty());
    assert!(!card.organizations.is_empty());
    // Google returns a structured `{year, month, day}`, normalized to JSContact text.
    let birthday = card.anniversaries.values().next().expect("birthday");
    assert_eq!(birthday.value.date, "1815-12-10");
    assert!(card.is_writable, "owned connections are writable");
    assert!(card.revisions.etag.is_some(), "the source etag is retained");
    assert!(card.raw_provider_json.is_some());
}

/// A delta that *does* carry a change reports it, and the response gains the
/// `totalItems`/`totalPeople` counters absent from the empty form.
#[tokio::test]
async fn captured_delta_reports_the_changed_person() {
    let client = fake_client(vec![("/v1/people/me/connections", json(DELTA))]);
    let cursor = SyncState::new("google-sync-token-4");
    let ContactSourceSync::Available { sync, .. } = GoogleContactProvider::connections(client)
        .sync_contacts(&account(), Some(&cursor))
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Delta { changed, removed } = sync.update else {
        panic!("expected a delta");
    };
    assert_eq!(changed.len(), 1);
    assert!(removed.is_empty());
    assert!(
        changed[0]
            .titles
            .values()
            .any(|title| title.value.name == "Principal Engineer"),
        "the patched title arrives through the delta"
    );
}

/// A deleted person returns as `metadata.deleted: true` carrying its `resourceName`
/// (plus a default photo and etag, but no name/email) — it must become a removal key.
#[tokio::test]
async fn captured_tombstone_becomes_a_removal_not_a_card() {
    let client = fake_client(vec![("/v1/people/me/connections", json(DELTA_REMOVED))]);
    let cursor = SyncState::new("google-sync-token-6");
    let ContactSourceSync::Available { sync, .. } = GoogleContactProvider::connections(client)
        .sync_contacts(&account(), Some(&cursor))
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Delta { changed, removed } = sync.update else {
        panic!("expected a delta");
    };
    assert!(changed.is_empty(), "a tombstone is never a card");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].as_str(), "people/contact-2");
}

/// Other Contacts is its own source with its own token; the captured page carries
/// `otherContacts` (not `connections`) and suggestion-class cards.
#[tokio::test]
async fn captured_other_contacts_normalize_as_suggestions() {
    let client = fake_client(vec![("/v1/otherContacts", json(OTHER_CONTACTS))]);
    let ContactSourceSync::Available { sync, .. } = GoogleContactProvider::other_contacts(client)
        .sync_contacts(&account(), None)
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    assert!(!objects.is_empty());
    assert!(
        objects.iter().all(|card| !card.is_writable),
        "Other Contacts are read-only"
    );
    assert!(
        objects
            .iter()
            .all(|card| card.source_class == engine_core::contact::ContactSourceClass::Suggested)
    );
}

/// Contact groups list without a sync token, so every pass is a snapshot keyed on the
/// static sentinel.
#[tokio::test]
async fn captured_contact_groups_are_a_snapshot_on_a_static_cursor() {
    let client = fake_client(vec![("/v1/contactGroups", json(CONTACT_GROUPS))]);
    let ContactSourceSync::Available { sync, .. } = GoogleContactProvider::groups(client)
        .sync_contacts(&account(), None)
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    assert!(matches!(sync.update, SyncUpdate::Snapshot { .. }));
    assert_eq!(sync.next_cursor.as_str(), "google-groups-snapshot");
}

/// **A consumer Google account refuses the Workspace directory with `400
/// FAILED_PRECONDITION` ("Must be a G Suite domain user"), not `403`.** The directory is
/// an optional source, so it must degrade to `Unavailable` rather than fail the sync.
#[tokio::test]
async fn captured_directory_refusal_degrades_instead_of_failing_the_sync() {
    let client = fake_client_fallible(vec![(
        "/v1/people:listDirectoryPeople",
        Err((400, json(DIRECTORY_PRECONDITION))),
    )]);
    let result = GoogleContactProvider::directory(client)
        .sync_contacts(&account(), None)
        .await
        .expect("an optional source refusal is not a sync failure");
    assert!(
        matches!(result, ContactSourceSync::Unavailable(_)),
        "consumer accounts have no directory; got {result:?}"
    );
}

/// The writable source is never optional: a failure there is a real error, never a
/// silent `Unavailable` that would look like an emptied address book.
#[tokio::test]
async fn owned_connections_never_degrade_to_unavailable() {
    let client = fake_client_fallible(vec![(
        "/v1/people/me/connections",
        Err((400, json(DIRECTORY_PRECONDITION))),
    )]);
    assert!(
        GoogleContactProvider::connections(client)
            .sync_contacts(&account(), None)
            .await
            .is_err(),
        "owned-contact failures must surface"
    );
}

/// A direct fetch normalizes the same person the snapshot produced.
#[tokio::test]
async fn captured_person_fetch_normalizes_like_a_snapshot_entry() {
    let client = fake_client(vec![("/v1/people/contact-", json(PERSON))]);
    let card = GoogleContactProvider::connections(client)
        .fetch_contact(
            &account(),
            &engine_core::ids::ContactId::try_from("people/contact-2").unwrap(),
        )
        .await
        .unwrap();
    assert!(
        card.name
            .as_ref()
            .and_then(|name| name.full.as_deref())
            .is_some_and(|full| full.contains("Lovelace"))
    );
    assert!(card.revisions.etag.is_some());
}

/// **A stale-etag `updateContact` is `400 FAILED_PRECONDITION`, not `412`.** It is still
/// a refetch-and-retry conflict, so it must not be classified as permanent — a host
/// would otherwise drop a recoverable outbox entry.
#[test]
fn a_stale_etag_write_is_a_conflict_not_a_permanent_failure() {
    let error = crate::error::GoogleError::status(400, STALE_ETAG);
    assert_eq!(
        error.failure_class(),
        engine_core::error::FailureClass::Conflict
    );
    // A 400 that is *not* a precondition failure stays permanent.
    assert_eq!(
        crate::error::GoogleError::status(400, SYNC_TOKEN_INVALID).failure_class(),
        engine_core::error::FailureClass::Permanent
    );
}

/// **Other Contacts accepts only a subset of `personFields`.** Asking for the full mask
/// fails the whole request with `400 INVALID_ARGUMENT`, so the suggested source must
/// send its own narrower `readMask`.
#[tokio::test]
async fn other_contacts_requests_only_the_fields_that_source_allows() {
    let (base, captured) = crate::test_support::capturing_server(
        "200 OK",
        r#"{"otherContacts":[],"nextSyncToken":"next"}"#,
    );
    let client = crate::GoogleClient::with_base("token", base, crate::test_support::tls()).unwrap();
    let _ = GoogleContactProvider::other_contacts(client)
        .sync_contacts(&account(), None)
        .await;
    let request = captured.recv().expect("captured request");
    assert!(request.contains("/v1/otherContacts?readMask="));
    // The fields People rejects for this source must never be requested.
    for rejected in [
        "nicknames",
        "addresses",
        "organizations",
        "birthdays",
        "biographies",
        "urls",
        "relations",
        "userDefined",
        "memberships",
    ] {
        assert!(
            !request.contains(rejected),
            "otherContacts readMask must not request {rejected}: {request}"
        );
    }
    for allowed in ["names", "emailAddresses", "phoneNumbers", "metadata"] {
        assert!(request.contains(allowed), "missing {allowed}");
    }
}

/// A `400 INVALID_ARGUMENT` — a genuinely malformed request — must **not** degrade to
/// `Unavailable`, even on an optional source: that would turn an adapter bug into a
/// silently empty address book. Only `FAILED_PRECONDITION`/`403` mean "no such source".
#[tokio::test]
async fn a_bad_request_on_an_optional_source_surfaces_instead_of_degrading() {
    let client = fake_client_fallible(vec![(
        "/v1/otherContacts",
        Err((400, json(SYNC_TOKEN_INVALID))),
    )]);
    assert!(
        GoogleContactProvider::other_contacts(client)
            .sync_contacts(&account(), None)
            .await
            .is_err(),
        "an INVALID_ARGUMENT must surface, not degrade"
    );
}
