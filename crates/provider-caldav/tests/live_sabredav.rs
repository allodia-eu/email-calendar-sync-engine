//! Gated live integration: the CalDAV sync loop against the **SabreDAV** harness
//! (`docker/sabredav/`) — a second, independent CalDAV implementation beside
//! Stalwart.
//!
//! SabreDAV diverges from Stalwart in exactly the ways real servers do (the
//! two-step RFC 6764 discovery, the `http://sabre.io/ns/sync/N` sync-token form,
//! collection naming), so passing the **same** seed assertions here proves the
//! client is not over-fit to one server. Seeded with the same six calendar
//! fixtures, the invariants match `live_caldav.rs`: six events, the master +
//! override fold, twelve occurrences (the weekly series = 7), and an idempotent
//! empty re-sync. Skips unless `SABREDAV_HTTP_ADDR` is set, so the offline
//! `cargo test --workspace` stays green.

use core::time::Duration;

use engine_core::{
    calendar::Event,
    ids::{AccountId, ProviderKey},
    sync::{SyncScope, SyncUpdate},
    time::TimeZoneId,
};
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::sync_calendar;
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use serde::de::DeserializeOwned;
use store_sqlite::SqliteStore;

mod common;

/// Reads the SabreDAV harness coordinates, or `None` to skip (offline gate).
fn harness() -> Option<(String, String, String)> {
    let addr = std::env::var("SABREDAV_HTTP_ADDR").ok()?;
    let user = std::env::var("SABREDAV_USER").unwrap_or_else(|_| "alice@test.local".to_owned());
    let pass = std::env::var("SABREDAV_PASS").unwrap_or_else(|_| "sabredav-alice-pw".to_owned());
    Some((addr, user, pass))
}

/// Connects, retrying briefly so a just-started container is tolerated.
async fn connect(addr: &str, user: &str, pass: &str) -> CalDavProvider {
    let config = CalDavConfig::new(
        format!("http://{addr}"),
        Credentials::Basic {
            username: user.to_owned(),
            password: pass.to_owned(),
        },
    );
    let mut last_err = None;
    for _ in 0..15 {
        match CalDavProvider::connect(config.clone()).await {
            Ok(provider) => return provider,
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect to SabreDAV harness: {last_err:?}");
}

async fn load<T: DeserializeOwned>(
    store: &SqliteStore<ManualClock>,
    scope: &SyncScope,
    key: &ProviderKey,
) -> T {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("deserialize stored object")
}

#[tokio::test]
async fn sabredav_calendar_sync_loop() {
    let Some((addr, user, pass)) = harness() else {
        eprintln!("skipping sabredav_calendar_sync_loop: SABREDAV_HTTP_ADDR unset");
        return;
    };
    // Serialize with the write round-trip (shares the calendar; see `live_caldav`).
    let _serial = common::serial_guard().await;
    let provider = connect(&addr, &user, &pass).await;

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("sabredav-live").unwrap();
    let horizon = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("sync_calendar");
    assert!(
        report.calendars.upserted >= 1,
        "the default calendar synced"
    );

    let event_scope = provider.event_scope(&account);
    let event_keys = store.object_keys(&event_scope).await.unwrap();
    assert_eq!(event_keys.len(), 6, "six seed calendar resources stored");

    let mut events = Vec::new();
    for key in &event_keys {
        events.push(load::<Event>(&store, &event_scope, key).await);
    }
    let by_uid = |uid: &str| events.iter().find(|e| e.uid.as_str() == uid).unwrap();
    assert_eq!(by_uid("meeting-2003@test.local").participants.len(), 3);
    assert_eq!(by_uid("virtual-2004@test.local").virtual_locations.len(), 1);
    assert!(by_uid("allday-2005@test.local").is_all_day());
    assert!(by_uid("floating-2006@test.local").start.is_floating());

    let weekly = by_uid("weekly-2002@test.local");
    assert!(weekly.is_recurring());
    assert!(weekly.recurrence_id.is_none());

    let mut total = 0;
    for key in &event_keys {
        total += store
            .index_row_counts(&event_scope, key)
            .await
            .unwrap()
            .occurrences;
    }
    assert_eq!(
        store
            .index_row_counts(&event_scope, weekly.id.key())
            .await
            .unwrap()
            .occurrences,
        7
    );
    assert_eq!(total, 12);

    // A second sync reuses the SabreDAV sync-token: an idempotent empty delta.
    let second = sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("second sync_calendar");
    assert_eq!(
        second.events.applied.upserted, 0,
        "no event changes on a re-sync"
    );
    assert_eq!(
        second.events.applied.tombstoned, 0,
        "nothing tombstoned on a re-sync"
    );
}

/// Connects to the SabreDAV harness, or `None` to skip (offline gate).
async fn write_provider(test: &str) -> Option<(CalDavProvider, AccountId)> {
    let Some((addr, user, pass)) = harness() else {
        eprintln!("skipping {test}: SABREDAV_HTTP_ADDR unset");
        return None;
    };
    let provider = connect(&addr, &user, &pass).await;
    Some((
        provider,
        AccountId::try_from("sabredav-write-live").unwrap(),
    ))
}

/// The same write lifecycle as `live_caldav.rs`, against the independent SabreDAV
/// server — proving conditional `PUT`/`DELETE` are not over-fit to Stalwart. Skips
/// with no `SABREDAV_HTTP_ADDR`.
#[tokio::test]
async fn sabredav_write_round_trip() {
    let Some((provider, account)) = write_provider("sabredav_write_round_trip").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::round_trip(&provider, &account).await;
}

/// The preservation claim, against the server that stores iCalendar **verbatim**.
/// Stalwart reserializes, so it can only show that no *content* was lost; SabreDAV keeps
/// the bytes, so here the patched document must come back exactly as the patcher wrote
/// it — the strictest form of the claim, and the one a byte-preserving server is
/// uniquely able to make.
#[tokio::test]
async fn sabredav_patched_update_preserves_the_document() {
    let Some((provider, account)) =
        write_provider("sabredav_patched_update_preserves_the_document").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::patched_update_preserves_the_document(&provider, &account).await;
}

/// SabreDAV's own `412`, and the same `Conflict` classification — the precondition is
/// not a Stalwart quirk.
#[tokio::test]
async fn sabredav_stale_if_match_is_a_conflict() {
    let Some((provider, account)) = write_provider("sabredav_stale_if_match_is_a_conflict").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::stale_if_match_is_a_conflict(&provider, &account).await;
}

/// SabreDAV likewise accepts a master + `RECURRENCE-ID` override in one resource.
#[tokio::test]
async fn sabredav_instance_override_split_is_accepted() {
    let Some((provider, account)) =
        write_provider("sabredav_instance_override_split_is_accepted").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::instance_override_split_is_accepted(&provider, &account).await;
}

/// The read-only half of the privilege pair (#61), which **only SabreDAV can prove**:
/// its seed gives Alice a calendar Bob owns and shares with her read-only, so one
/// `PROPFIND` of one calendar home returns two collections with two different answers to
/// "what may I do here". Stalwart's harness account owns everything it can see.
///
/// Before `DAV:current-user-privilege-set` was requested, *both* came back `may_write`,
/// and a host gating an edit button on that was told "yes, you may" about a collection
/// whose `PUT` is a `403`.
#[tokio::test]
async fn sabredav_reports_a_read_only_share_as_unwritable() {
    let Some((provider, account)) =
        write_provider("sabredav_reports_a_read_only_share_as_unwritable").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;

    let synced = provider
        .sync_calendars(&account, None)
        .await
        .expect("sync_calendars");
    let calendars = match synced.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    };
    let by_href = |suffix: &str| {
        calendars
            .iter()
            .find(|calendar| calendar.id.as_str().ends_with(suffix))
            .unwrap_or_else(|| panic!("a collection at {suffix} is listed"))
    };

    // Alice's own calendar: SabreDAV grants her DAV:write.
    assert!(by_href("/default/").access.may_write);

    // Bob's, shared read-only: she gets `read` and `write-properties` but no `write` —
    // so no event may be written into it, and the engine must not claim otherwise.
    let shared = by_href("/bob-readonly/");
    assert!(
        !shared.access.may_write,
        "a read-only share must not report may_write"
    );
    assert!(shared.access.may_read, "she can still read it");
}

/// Read-your-writes (#65): a write reconciles the **store** to the server's copy, so a
/// host can re-read what it wrote — and guard its next edit on the revision the server
/// actually reported, instead of a `412` on the superseded one it wrote over.
#[tokio::test]
async fn sabredav_write_reconciles_the_store() {
    let Some((provider, account)) = write_provider("sabredav_write_reconciles_the_store").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::reconcile::read_your_writes(&provider, &account).await;
}
