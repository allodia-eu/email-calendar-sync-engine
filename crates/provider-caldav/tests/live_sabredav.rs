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

use engine_core::calendar::Event;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::SyncScope;
use engine_core::time::TimeZoneId;
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::sync_calendar;
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use serde::de::DeserializeOwned;
use store_sqlite::SqliteStore;

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
    assert_eq!(second.events.upserted, 0, "no event changes on a re-sync");
    assert_eq!(
        second.events.tombstoned, 0,
        "nothing tombstoned on a re-sync"
    );
}
