//! Gated live integration: the **CalDAV calendar sync loop** against the Stalwart
//! harness.
//!
//! Drives `engine-sync` with the real `CalDavProvider` into a real `SqliteStore`,
//! then asserts the calendar seed *in the store*: the six fixtures normalize, the
//! recurring resource's master + `RECURRENCE-ID` override fold into one event with
//! an `EXDATE` exclusion, participants merge, the virtual location survives, and
//! every event materializes occurrences. A second sync proves the held sync-token
//! yields an idempotent empty delta. Skips with no `STALWART_HTTP_ADDR`, so the
//! offline `cargo test --workspace` stays green.
//!
//! Per the determinism rule, every assertion is on harness-controlled content
//! (iCalendar UIDs, titles, counts) — never on the server-assigned hrefs, ETags,
//! or sync-tokens.

use core::time::Duration;
use std::time::Duration as StdDuration;

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
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

mod common;

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
async fn caldav_calendar_sync_loop() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping caldav_calendar_sync_loop: STALWART_HTTP_ADDR unset");
        return;
    };
    // Serialize with the write round-trip: it transiently adds an event, which
    // would otherwise race this test's exact event-count assertion.
    let _serial = common::serial_guard().await;
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let provider = CalDavProvider::connect(CalDavConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::Basic {
            username: harness.account.clone(),
            password: harness.password.clone(),
        },
    ))
    .await
    .expect("connect + discover");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("caldav-live").unwrap();
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

    // The one-off zoned event, the meeting's three merged participants, the
    // virtual location, and the zoneless all-day event.
    assert_eq!(
        by_uid("oneoff-2001@test.local").title,
        "One-off zoned event"
    );
    assert_eq!(by_uid("meeting-2003@test.local").participants.len(), 3);
    assert_eq!(by_uid("virtual-2004@test.local").virtual_locations.len(), 1);
    assert!(by_uid("allday-2005@test.local").is_all_day());
    assert!(by_uid("floating-2006@test.local").start.is_floating());

    // The recurring resource folded master + override into one recurring event.
    let weekly = by_uid("weekly-2002@test.local");
    assert!(weekly.is_recurring());
    assert!(weekly.recurrence_id.is_none());

    // Occurrences materialized: weekly = 8 instances − 1 EXDATE = 7; 12 in total.
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

    // A second sync reuses the held sync-token: an idempotent, empty delta.
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
    assert_eq!(
        store.object_keys(&event_scope).await.unwrap().len(),
        6,
        "the event set is unchanged after the delta"
    );
}

/// The full CalDAV write lifecycle against the real Stalwart: create → update
/// (`If-Match`) → delete (`If-Match`), verified by re-reading the collection. Leaves
/// the seed untouched. Skips with no `STALWART_HTTP_ADDR`.
#[tokio::test]
async fn caldav_write_round_trip() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping caldav_write_round_trip: STALWART_HTTP_ADDR unset");
        return;
    };
    let _serial = common::serial_guard().await;
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let provider = CalDavProvider::connect(CalDavConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::Basic {
            username: harness.account.clone(),
            password: harness.password.clone(),
        },
    ))
    .await
    .expect("connect + discover");

    let account = AccountId::try_from("caldav-write-live").unwrap();
    common::write_round_trip(&provider, &account).await;
}
