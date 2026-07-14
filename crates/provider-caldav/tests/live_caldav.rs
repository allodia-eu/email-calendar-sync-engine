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

/// Connects a provider to the live Stalwart harness, or `None` to skip (offline gate).
async fn connect(test: &str) -> Option<(CalDavProvider, AccountId)> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_HTTP_ADDR unset");
        return None;
    };
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
    Some((provider, AccountId::try_from("caldav-write-live").unwrap()))
}

/// The full CalDAV write lifecycle against the real Stalwart, driven off the `ETag`s the
/// `PUT`s return. Leaves the seed untouched. Skips with no `STALWART_HTTP_ADDR`.
#[tokio::test]
async fn caldav_write_round_trip() {
    let Some((provider, account)) = connect("caldav_write_round_trip").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::round_trip(&provider, &account).await;
}

/// The headline of #62: an edit made with the structural patcher (#58) survives the real
/// server. Stalwart **reserializes** what it stores — it re-folds content lines and
/// reorders `RRULE` parts — so this is the server that proves the preservation claim is
/// about content, not bytes on the wire.
#[tokio::test]
async fn caldav_patched_update_preserves_the_document() {
    let Some((provider, account)) = connect("caldav_patched_update_preserves_the_document").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::patched_update_preserves_the_document(&provider, &account).await;
}

/// A superseded `If-Match` really does come back `412` from Stalwart, and the adapter
/// classes it `Conflict` — the input the outbox needs to refetch-and-merge instead of
/// blindly retrying.
#[tokio::test]
async fn caldav_stale_if_match_is_a_conflict() {
    let Some((provider, account)) = connect("caldav_stale_if_match_is_a_conflict").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::stale_if_match_is_a_conflict(&provider, &account).await;
}

/// Stalwart accepts a resource carrying a master **and** a `RECURRENCE-ID` override the
/// patcher split out of it, and hands it back folded into one event.
#[tokio::test]
async fn caldav_instance_override_split_is_accepted() {
    let Some((provider, account)) = connect("caldav_instance_override_split_is_accepted").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::instance_override_split_is_accepted(&provider, &account).await;
}

/// Stalwart reports `DAV:current-user-privilege-set` on Alice's own calendar, and it
/// grants `DAV:write` — so the collection the write tests above target reports itself
/// writable. The read-only half of this pair is SabreDAV's shared calendar
/// (`live_sabredav.rs`); Stalwart's harness account owns everything it can see, so it
/// cannot produce a collection it may not write.
#[tokio::test]
async fn caldav_reports_the_bound_calendar_as_writable() {
    let Some((provider, account)) = connect("caldav_reports_the_bound_calendar_as_writable").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;

    let calendars = provider
        .sync_calendars(&account, None)
        .await
        .expect("sync_calendars");
    let listed = match calendars.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    };
    let bound = listed
        .iter()
        .find(|calendar| calendar.id.as_str() == provider.collection_href())
        .expect("the bound collection is listed");
    assert!(
        bound.access.may_write,
        "Stalwart grants DAV:write on the account's own calendar"
    );
    assert!(bound.access.may_read);
}
