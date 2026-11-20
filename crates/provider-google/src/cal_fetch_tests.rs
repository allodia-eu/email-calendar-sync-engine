//! Offline calendar fetch/paging tests against the captured calendar fixtures.

use engine_core::time::CalendarDate;
use engine_provider::{PageToken, SyncKind};

use super::*;
use crate::test_support::{fake_client, fake_client_fallible, json};

const CALENDARS: &str = include_str!("../tests/fixtures/calendar/calendars.json");
const EVENTS: &str = include_str!("../tests/fixtures/calendar/events_list.json");
const DELTA: &str = include_str!("../tests/fixtures/calendar/events_delta.json");
const SYNC_GONE: &str = include_str!("../tests/fixtures/error/sync_token_gone.json");

fn calendar() -> CalendarId {
    CalendarId::try_from("primary").unwrap()
}

#[tokio::test]
async fn calendars_map_the_list_snapshot() {
    let client = fake_client(vec![("/calendarList", json(CALENDARS))]);
    let calendars = calendars(&client).await.unwrap();
    assert_eq!(calendars.len(), 2);
    assert!(calendars.iter().any(|c| c.is_default));
}

#[tokio::test]
async fn events_snapshot_projects_masters_and_singles_with_a_sync_token() {
    let client = fake_client(vec![("/events?singleEvents=false", json(EVENTS))]);
    let page = events_page(&client, &calendar(), None, None, None)
        .await
        .unwrap()
        .page;
    assert_eq!(page.kind, SyncKind::Snapshot);
    // All five captured events are masters/singles → kept and present.
    assert_eq!(page.changed.len(), 5);
    assert_eq!(page.present.len(), 5);
    assert_eq!(page.removed, []);
    assert_eq!(page.next_cursor.as_str(), "v1;events-sync-token-1");
    // Each id names its calendar.
    assert!(
        page.changed
            .iter()
            .all(|event| event.id.as_str().starts_with("primary/"))
    );
    assert!(page.next_page.is_none());
}

#[tokio::test]
async fn events_delta_tombstones_cancelled_and_advances_the_cursor() {
    let client = fake_client(vec![("syncToken=", json(DELTA))]);
    let page = events_page(
        &client,
        &calendar(),
        Some(&SyncState::new("v1;events-sync-token-1")),
        None,
        None,
    )
    .await
    .unwrap()
    .page;
    assert_eq!(page.kind, SyncKind::Delta);
    // One updated event; one cancelled → a tombstone.
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.removed.len(), 1);
    assert!(
        page.removed[0].as_str().starts_with("primary/"),
        "{:?}",
        page.removed
    );
    assert_eq!(page.present, []);
    assert_eq!(page.next_cursor.as_str(), "v1;events-sync-token-2");
}

/// A cursor from before event ids named their calendar is read as none: a delta on it would
/// store every changed event a second time, beside its old id.
#[tokio::test]
async fn a_cursor_from_before_qualified_ids_syncs_the_calendar_afresh() {
    let client = fake_client(vec![("/events?singleEvents=false", json(EVENTS))]);
    let page = events_page(
        &client,
        &calendar(),
        Some(&SyncState::new("events-sync-token-1")),
        None,
        None,
    )
    .await
    .unwrap()
    .page;
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.present.len(), 5);
    assert_eq!(page.next_cursor.as_str(), "v1;events-sync-token-1");
}

#[tokio::test]
async fn a_stale_sync_token_is_needs_resync() {
    let client = fake_client_fallible(vec![("syncToken=", Err((410, json(SYNC_GONE))))]);
    let err = events_page(
        &client,
        &calendar(),
        Some(&SyncState::new("v1;old")),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::NeedsResync
    );
}

#[tokio::test]
async fn events_snapshot_sets_an_override_aside_and_holds_a_pending_cursor() {
    // A page with only a per-instance override (recurringEventId) and no nextSyncToken:
    // the override is dropped and the cursor stays the pending placeholder (a mid-drain
    // page), with a nextPageToken to continue.
    let doc = serde_json::json!({
        "timeZone": "Europe/Amsterdam",
        "nextPageToken": "P2",
        "items": [
            { "id": "master-1", "summary": "Series",
              "recurrence": ["RRULE:FREQ=DAILY;COUNT=3"],
              "start": { "dateTime": "2026-08-03T09:00:00+02:00", "timeZone": "Europe/Amsterdam" },
              "end": { "dateTime": "2026-08-03T09:30:00+02:00", "timeZone": "Europe/Amsterdam" } },
            { "id": "inst-1", "summary": "Override", "recurringEventId": "master-1", "status": "confirmed",
              "originalStartTime": { "dateTime": "2026-08-04T09:00:00+02:00", "timeZone": "Europe/Amsterdam" },
              "start": { "dateTime": "2026-08-04T10:00:00+02:00", "timeZone": "Europe/Amsterdam" },
              "end": { "dateTime": "2026-08-04T10:30:00+02:00", "timeZone": "Europe/Amsterdam" } }
        ]
    });
    let client = fake_client(vec![("/events?singleEvents=false", doc)]);
    let read = events_page(&client, &calendar(), None, None, None)
        .await
        .unwrap();
    // The override is not an object of its own — it is collected, to be folded into the
    // series once every page of the pass is in (the master could be on any of them).
    assert_eq!(read.overrides.len(), 1);
    let page = read.page;
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.changed[0].id.as_str(), "primary/master-1");
    // No nextSyncToken on this intermediate page → the pending placeholder cursor.
    assert!(page.next_cursor.as_str().contains("pending"));
    assert!(page.next_page.is_some());
}

#[test]
fn page_urls_carry_the_window_sync_and_page_tokens() {
    let client = fake_client(vec![]);
    let window = CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 9, 1).unwrap(),
    );
    // The initial snapshot carries singleEvents=false and, when set, the window.
    let first = page_url(&client, &calendar(), None, None, Some(window));
    assert!(first.contains("/calendars/primary/events?singleEvents=false"));
    assert!(first.contains("&timeMin=2026-08-01T00:00:00Z&timeMax=2026-09-01T00:00:00Z"));
    // A delta carries the syncToken (never a window — Google forbids combining them).
    let delta = page_url(&client, &calendar(), Some("TOK"), None, None);
    assert!(delta.contains("?syncToken=TOK") && !delta.contains("timeMin"));
    // A continuation carries the pageToken.
    let cont = page_url(
        &client,
        &calendar(),
        None,
        Some(&PageToken::new("P2")),
        None,
    );
    assert!(cont.contains("?pageToken=P2"));
}

#[test]
fn a_calendar_id_is_one_path_segment_however_it_is_spelled() {
    let client = fake_client(vec![]);
    let holidays = CalendarId::try_from("nl.dutch#holiday@group.v.calendar.google.com").unwrap();
    let url = page_url(&client, &holidays, None, None, None);
    assert!(
        url.contains("/calendars/nl.dutch%23holiday%40group.v.calendar.google.com/events?"),
        "{url}"
    );
    assert!(!url.contains('#'), "{url}");
}

#[tokio::test]
async fn a_captured_series_comes_back_with_its_overrides_folded_in() {
    // Captured from a real calendar: a weekly series, one occurrence moved to the afternoon
    // and renamed, one deleted. Both arrive as entries of their own, and neither is an
    // object this engine stores — they are exceptions *of* the series, so what the pass
    // hands over is one event carrying both.
    const SERIES: &str =
        include_str!("../tests/fixtures/calendar/events_series_with_overrides.json");

    let client = fake_client(vec![("/events?singleEvents=false", json(SERIES))]);
    let read = events_page(&client, &calendar(), None, None, None)
        .await
        .unwrap();
    assert_eq!(read.page.changed.len(), 1, "one event, not three");
    assert!(
        read.page.removed.is_empty(),
        "a cancelled occurrence is an exclusion on its series, never a tombstone for an \
         object the store was never given"
    );

    let mut events = read.page.changed;
    crate::cal_override::fold_into(&mut events, read.overrides);
    let recurrence = events[0].recurrence.as_ref().expect("a series");

    let engine_core::calendar::RecurrenceOverride::Patch(patch) = recurrence
        .overrides
        .get(&"2026-09-14T09:30:00".parse().unwrap())
        .expect("the moved occurrence, keyed by the start it had")
    else {
        panic!("a moved occurrence is a patch, not an exclusion");
    };
    assert_eq!(patch.get("start").unwrap(), "2026-09-14T14:00:00");
    assert_eq!(patch.get("duration").unwrap(), "PT45M");
    assert_eq!(patch.get("title").unwrap(), "Moved to the afternoon");

    assert_eq!(
        recurrence
            .overrides
            .get(&"2026-09-21T09:30:00".parse().unwrap()),
        Some(&engine_core::calendar::RecurrenceOverride::Excluded),
        "and the deleted one is excluded — even though the entry still carries its old \
         start, end and title, which reading it as a patch would have pinned"
    );
}
