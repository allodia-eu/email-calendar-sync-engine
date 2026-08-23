//! Offline calendar-provider tests: `sync_calendars` and the snapshot/delta/410-restart
//! `sync_events` drain.

use engine_core::sync::SyncUpdate;

use super::*;
use crate::test_support::{fake_client, fake_client_fallible, json};

const CALENDARS: &str = include_str!("../tests/fixtures/calendar/calendars.json");
const EVENTS: &str = include_str!("../tests/fixtures/calendar/events_list.json");
const DELTA: &str = include_str!("../tests/fixtures/calendar/events_delta.json");
const SYNC_GONE: &str = include_str!("../tests/fixtures/error/sync_token_gone.json");

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn calendar() -> CalendarId {
    CalendarId::try_from("primary").unwrap()
}

fn provider(routes: Vec<(&'static str, serde_json::Value)>) -> GoogleCalendarProvider {
    GoogleCalendarProvider::new(fake_client(routes), calendar())
}

#[test]
fn scopes_bind_a_calendar_and_a_calendar_list_container() {
    let p = provider(vec![]);
    assert_eq!(
        p.event_scope(&account()),
        SyncScope::GoogleCalendar {
            account: account(),
            calendar: calendar()
        }
    );
    assert_eq!(
        p.calendar_scope(&account()),
        SyncScope::GoogleCalendarList { account: account() }
    );
    assert!(p.connection_info().capabilities.calendars());
}

#[tokio::test]
async fn sync_calendars_snapshots_the_list() {
    let sync = provider(vec![("/calendarList", json(CALENDARS))])
        .sync_calendars(&account(), None)
        .await
        .unwrap();
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("expected a calendar snapshot");
    };
    assert_eq!(objects.len(), 2);
    assert_eq!(present.len(), 2);
}

#[tokio::test]
async fn sync_events_snapshot_reconciles_with_a_sync_token() {
    let sync = provider(vec![("/events?singleEvents=false", json(EVENTS))])
        .sync_events(&account(), None)
        .await
        .unwrap();
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected a snapshot");
    };
    assert_eq!(objects.len(), 5);
    assert_eq!(sync.next_cursor.as_str(), "events-sync-token-1");
}

#[tokio::test]
async fn sync_events_delta_is_additive_with_a_tombstone() {
    let sync = provider(vec![("syncToken=", json(DELTA))])
        .sync_events(&account(), Some(&SyncState::new("events-sync-token-1")))
        .await
        .unwrap();
    assert!(!sync.is_snapshot());
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &sync.update
    else {
        panic!("expected a delta");
    };
    assert_eq!(changed.len(), 1);
    assert_eq!(removed.len(), 1);
    assert_eq!(sync.next_cursor.as_str(), "events-sync-token-2");
}

#[test]
fn with_window_and_write_capabilities_and_debug() {
    use engine_core::time::CalendarDate;
    let window = crate::CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 9, 1).unwrap(),
    );
    let p = provider(vec![]).with_window(window);
    // Google enforces the If-Match guard → writes advertised.
    assert!(p.connection_info().capabilities.calendar_writes());
    assert!(format!("{p:?}").contains("GoogleCalendarProvider"));
}

#[tokio::test]
async fn create_patch_delete_route_through_the_provider() {
    use engine_core::{
        calendar::Event,
        ids::Uid,
        membership::Memberships,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
        version::{ETag, RevisionTokens},
    };
    use engine_provider::{EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget};

    let stamp: UtcDateTime = "2026-07-18T10:00:00Z".parse().unwrap();
    let zoned = CalendarDateTime::Zoned {
        local: "2026-08-03T09:00:00".parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };
    let created = serde_json::json!({ "id": "srv-1", "iCalUID": "u@google.com", "etag": "\"v1\"" });
    let p = provider(vec![
        ("/events", created.clone()),
        ("/events/srv-1", created),
    ]);

    // A draft for the *bound* calendar is created; one for another calendar is refused.
    let draft = EventDraft::new(
        calendar(),
        Uid::new("u@test.local").unwrap(),
        "Meeting",
        zoned.clone(),
        zoned.clone(),
        stamp,
    );
    let receipt = p.create_event(&account(), &draft).await.unwrap();
    assert_eq!(receipt.event.key().as_str(), "srv-1");
    let elsewhere = EventDraft::new(
        CalendarId::try_from("other").unwrap(),
        Uid::new("u@test.local").unwrap(),
        "Nope",
        zoned.clone(),
        zoned.clone(),
        stamp,
    );
    assert!(p.create_event(&account(), &elsewhere).await.is_err());

    // Patch + delete route through too.
    let mut base = Event::new(
        engine_core::ids::EventId::try_from("srv-1").unwrap(),
        Uid::new("u@google.com").unwrap(),
        Memberships::of_one(calendar()),
        zoned,
    );
    base.revisions = RevisionTokens::from_etag(ETag::new("\"v1\""));
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp).summary("x"),
    );
    assert!(p.patch_event(&account(), &base, &edit).await.is_ok());
    assert!(
        p.delete_event(&account(), None, &EventDeletion::of(&base))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn sync_events_restarts_as_a_snapshot_when_the_sync_token_expired() {
    let mut routes: Vec<(&str, crate::test_support::FakeRoute)> =
        vec![("syncToken=", Err((410, json(SYNC_GONE))))];
    routes.push(("/events?singleEvents=false", Ok(json(EVENTS))));
    let sync = GoogleCalendarProvider::new(fake_client_fallible(routes), calendar())
        .sync_events(&account(), Some(&SyncState::new("old")))
        .await
        .unwrap();
    // Recovery yields a fresh snapshot, not an error.
    assert!(sync.is_snapshot());
    assert_eq!(sync.next_cursor.as_str(), "events-sync-token-1");
}
