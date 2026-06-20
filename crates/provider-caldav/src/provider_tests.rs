//! `CalDavProvider` tests: scope/capability wiring and the **full offline sync
//! loop** — the real provider (with a fake executor replaying captured Stalwart
//! transcripts) driven through `engine_sync::sync_calendar` into a real
//! `SqliteStore`, asserting the seed normalizes, the master+override folds, and
//! occurrences materialize. This is the CalDAV analogue of `provider-jmap`'s
//! `live_sync`, but deterministic and Docker-free.

use core::time::Duration;

use engine_core::calendar::{Calendar, Event};
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::SyncScope;
use engine_core::time::TimeZoneId;
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::sync_calendar;
use serde::de::DeserializeOwned;
use store_sqlite::SqliteStore;

use super::*;
use crate::test_support::Replay;

fn replay(bodies: Vec<&str>) -> Replay {
    Replay::bodies(bodies)
}

const PRINCIPAL: &str = include_str!("../tests/fixtures/principal.xml");
const HOME: &str = include_str!("../tests/fixtures/calendar-home.xml");
const SYNC_INITIAL: &str = include_str!("../tests/fixtures/sync-initial.xml");

async fn connect(exec: Replay) -> CalDavProvider {
    CalDavProvider::with_executor(Box::new(exec), "/.well-known/caldav", "default")
        .await
        .expect("discovery")
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

#[test]
fn resolves_relative_and_absolute_collections() {
    assert_eq!(
        resolve_collection("/dav/cal/u/", "default"),
        "/dav/cal/u/default/"
    );
    assert_eq!(resolve_collection("/dav/cal/u", "work"), "/dav/cal/u/work/");
    // An absolute collection path is used verbatim (with a trailing slash).
    assert_eq!(
        resolve_collection("/dav/cal/u/", "/shared/team/"),
        "/shared/team/"
    );
    // A full-URL href (as some servers return) passes through unchanged.
    assert_eq!(
        resolve_collection("/dav/cal/u/", "https://dav.example.com/cal/x/"),
        "https://dav.example.com/cal/x/"
    );
}

#[tokio::test]
async fn exposes_dav_scopes_and_only_the_calendar_capability() {
    let provider = connect(replay(vec![PRINCIPAL])).await;
    let account = AccountId::try_from("a").unwrap();

    assert!(provider.capabilities().calendars());
    assert!(!provider.capabilities().mail());
    assert!(!provider.capabilities().submission());

    assert_eq!(
        provider.calendar_scope(&account),
        SyncScope::DavCollectionList {
            account: account.clone()
        }
    );
    match provider.event_scope(&account) {
        SyncScope::DavCollection { collection, .. } => {
            assert_eq!(collection.as_str(), "/dav/cal/alice%40test.local/default/");
        }
        other => panic!("expected a DavCollection scope, got {other:?}"),
    }
    assert_eq!(
        provider.collection_href(),
        "/dav/cal/alice%40test.local/default/"
    );
}

// One cohesive end-to-end flow (discover → list calendars → sync events → assert
// normalization + occurrences); splitting it would obscure the single scenario.
#[tokio::test]
async fn calendar_sync_loop_normalizes_folds_and_expands_the_seed() {
    let provider = connect(replay(vec![PRINCIPAL, HOME, SYNC_INITIAL])).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("caldav-acct").unwrap();
    let horizon = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("t"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("sync_calendar");

    // ---- Calendars: the one default collection, applied as a container. ----
    let calendar_scope = provider.calendar_scope(&account);
    let calendar_keys = store.object_keys(&calendar_scope).await.unwrap();
    assert_eq!(calendar_keys.len(), 1);
    let calendar: Calendar = load(&store, &calendar_scope, &calendar_keys[0]).await;
    assert_eq!(calendar.name, "Stalwart Calendar (alice@test.local)");

    // ---- Events: six seed resources, each a member of the bound calendar. ----
    let event_scope = provider.event_scope(&account);
    let event_keys = store.object_keys(&event_scope).await.unwrap();
    assert_eq!(event_keys.len(), 6, "six seed resources stored");

    let mut events = Vec::new();
    for key in &event_keys {
        events.push(load::<Event>(&store, &event_scope, key).await);
    }
    assert!(
        events.iter().all(|e| e.calendars.contains(&calendar.id)),
        "every event references the bound calendar (referential integrity)"
    );

    // The meeting normalized its merged participants and the virtual location.
    let meeting = events
        .iter()
        .find(|e| e.uid.as_str() == "meeting-2003@test.local")
        .unwrap();
    assert_eq!(meeting.participants.len(), 3);
    let virtual_event = events
        .iter()
        .find(|e| e.uid.as_str() == "virtual-2004@test.local")
        .unwrap();
    assert_eq!(virtual_event.virtual_locations.len(), 1);

    // The recurring resource folded its master + RECURRENCE-ID override into one
    // recurring event whose raw iCalendar is preserved.
    let weekly = events
        .iter()
        .find(|e| e.uid.as_str() == "weekly-2002@test.local")
        .unwrap();
    assert!(weekly.is_recurring());
    assert!(weekly.recurrence_id.is_none());
    assert!(
        weekly
            .raw_ical
            .as_ref()
            .unwrap()
            .as_str()
            .contains("RECURRENCE-ID")
    );

    // ---- Occurrences: every event materializes, recurrence honored end to end. ----
    let mut total = 0;
    for key in &event_keys {
        total += store
            .index_row_counts(&event_scope, key)
            .await
            .unwrap()
            .occurrences;
    }
    let weekly_occurrences = store
        .index_row_counts(&event_scope, weekly.id.key())
        .await
        .unwrap()
        .occurrences;
    // 8 weekly instances − 1 EXDATE = 7 (the moved RECURRENCE-ID instance is still
    // one occurrence, just at its overridden time).
    assert_eq!(weekly_occurrences, 7);
    // oneoff(1) + weekly(7) + meeting(1) + virtual(1) + all-day(1) + floating(1).
    assert_eq!(total, 12);
}

#[tokio::test]
async fn calendar_list_includes_a_bound_collection_outside_the_home() {
    // A provider bound to an absolute collection NOT under the discovered home:
    // sync_calendars must still represent it, so events synced under it never
    // reference a calendar the container snapshot omits.
    // PRINCIPAL drives discovery; HOME is the calendar-list response (it lists only
    // the default collection, NOT /shared/team-calendar/).
    let provider = CalDavProvider::with_executor(
        Box::new(replay(vec![PRINCIPAL, HOME])),
        "/.well-known/caldav",
        "/shared/team-calendar/",
    )
    .await
    .expect("discovery");
    let account = AccountId::try_from("acct").unwrap();

    let listed = provider
        .sync_calendars(&account, None)
        .await
        .expect("sync_calendars");
    let objects = match &listed.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { .. } => panic!("calendar list is a snapshot"),
    };
    assert!(
        objects
            .iter()
            .any(|c| c.id.as_str() == "/shared/team-calendar/"),
        "the bound out-of-home collection is represented in the container snapshot"
    );
    // The list cursor is the named sentinel, never the empty string.
    assert_eq!(listed.next_cursor.as_str(), "caldav-calendar-list");
}

#[tokio::test]
async fn rebind_switches_collection_without_rediscovery() {
    // connect runs discovery once; rebind reuses the home + executor with no extra
    // PROPFIND, only moving the bound collection.
    let provider = connect(replay(vec![PRINCIPAL])).await;
    let account = AccountId::try_from("acct").unwrap();
    let rebound = provider.rebind("/calendars/other/").expect("rebind");
    match rebound.event_scope(&account) {
        SyncScope::DavCollection { collection, .. } => {
            assert_eq!(collection.as_str(), "/calendars/other/");
        }
        other => panic!("expected a DavCollection scope, got {other:?}"),
    }
    assert_eq!(rebound.collection_href(), "/calendars/other/");
}
