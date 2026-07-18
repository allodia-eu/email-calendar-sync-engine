//! Offline tests for calendar-list + `calendarView/delta` fetch, driven by the fake
//! transport over scrubbed real Graph responses.

use engine_provider::SyncKind;
use serde_json::json as sjson;

use super::*;
use crate::test_support::{fake_client, fake_client_fallible, json};

const CALENDARS: &str = include_str!("../tests/fixtures/calendar/calendars.json");
const DELTA: &str = include_str!("../tests/fixtures/calendar/events_delta.json");

fn calendar_id() -> CalendarId {
    CalendarId::try_from("cal-1").unwrap()
}

fn window() -> CalendarWindow {
    CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 11, 1).unwrap(),
    )
}

#[tokio::test]
async fn calendars_snapshot_projects_the_calendar_list() {
    let client = fake_client(vec![("/calendars?$top", json(CALENDARS))]);
    let calendars = calendars(&client).await.unwrap();
    // A single MS account exposes many calendars; each is a distinct container.
    assert_eq!(calendars.len(), 2);
    assert_eq!(calendars[0].name, "Calendar");
    assert!(calendars[0].is_default);
    // The non-default "Extra calendar test" keeps its own id, name, and `#rrggbb` color.
    let extra = &calendars[1];
    assert_eq!(extra.name, "Extra calendar test");
    assert!(!extra.is_default);
    assert_eq!(extra.color.as_deref(), Some("#f7630c"));
    assert_ne!(extra.id, calendars[0].id);
}

#[tokio::test]
async fn events_snapshot_keeps_masters_and_singles_and_drops_occurrences_and_exceptions() {
    // The fixture delta carries a master + 2 singles + 2 occurrences + 1 exception; only
    // the master and singles are projected (the engine expands the master locally, and a
    // Graph exception has no recoverable recurrence-id).
    let client = fake_client(vec![("calendarView/delta", json(DELTA))]);
    let page = events_page(
        &client,
        &calendar_id(),
        None,
        None,
        window(),
        "Europe/Amsterdam",
    )
    .await
    .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.changed.len(), 3, "master + 2 singles kept");
    assert_eq!(page.present.len(), 3);
    assert!(page.removed.is_empty());
    // Exactly one of the kept events is the recurring master.
    assert_eq!(page.changed.iter().filter(|e| e.is_recurring()).count(), 1);
    // The pass ends at the deltaLink, which becomes the persisted cursor.
    assert!(page.next_cursor.as_str().contains("deltatoken"));
}

#[tokio::test]
async fn a_delta_tombstones_a_removed_event() {
    let cursor = SyncState::new("https://graph.test/me/calendars/cal-1/calendarView/delta?token=1");
    let removed = sjson!({
        "@odata.deltaLink": "https://graph.test/me/calendarView/delta?$deltatoken=next",
        "value": [ { "id": "evt-gone", "@removed": { "reason": "deleted" } } ]
    });
    let client = fake_client(vec![("token=1", removed)]);
    let page = events_page(
        &client,
        &calendar_id(),
        Some(&cursor),
        None,
        window(),
        "Europe/Amsterdam",
    )
    .await
    .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert!(page.changed.is_empty());
    assert_eq!(page.removed.len(), 1);
    assert_eq!(page.removed[0].as_str(), "evt-gone");
    assert!(page.next_cursor.as_str().contains("deltatoken"));
}

#[tokio::test]
async fn the_initial_url_carries_the_calendar_and_the_window() {
    // page_url builds the per-calendar, windowed calendarView/delta on the first call.
    let client = fake_client(vec![]);
    let url = page_url(&client, &calendar_id(), None, None, window());
    assert!(url.contains("/calendars/cal-1/calendarView/delta"), "{url}");
    assert!(url.contains("startDateTime=2026-08-01T00:00:00Z"), "{url}");
    assert!(url.contains("endDateTime=2026-11-01T00:00:00Z"), "{url}");
    // A continuation cursor is followed verbatim (it already encodes the window).
    let cursor = SyncState::new("https://graph.test/me/calendarView/delta?$deltatoken=x");
    assert_eq!(
        page_url(&client, &calendar_id(), Some(&cursor), None, window()),
        cursor.as_str()
    );
}

#[tokio::test]
async fn a_response_without_a_value_array_is_a_protocol_error() {
    let client = fake_client_fallible(vec![("calendarView/delta", Ok(sjson!({ "nope": true })))]);
    assert!(
        events_page(
            &client,
            &calendar_id(),
            None,
            None,
            window(),
            "Europe/Amsterdam"
        )
        .await
        .is_err()
    );
}
