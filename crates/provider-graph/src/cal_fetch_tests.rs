//! Offline tests for calendar-list + `calendarView/delta` fetch, driven by the fake
//! transport over scrubbed real Graph responses.

use engine_core::{calendar::RecurrenceOverride, time::LocalDateTime};
use engine_provider::SyncKind;
use serde_json::json as sjson;

use super::*;
use crate::{
    cal_override,
    test_support::{fake_client, fake_client_fallible, fake_client_recording, json},
};

const CALENDARS: &str = include_str!("../tests/fixtures/calendar/calendars.json");
const DELTA: &str = include_str!("../tests/fixtures/calendar/events_delta.json");
const MASTER: &str = include_str!("../tests/fixtures/calendar/event_master_cancellations.json");

fn calendar_id() -> CalendarId {
    CalendarId::try_from("cal-1").unwrap()
}

fn window() -> CalendarWindow {
    CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 11, 1).unwrap(),
    )
}

/// The routes a delta pass needs: the page itself, and the per-master re-read it fans out.
fn delta_routes() -> Vec<(&'static str, Value)> {
    vec![
        ("calendarView/delta", json(DELTA)),
        ("$select=start,end,cancelledOccurrences", json(MASTER)),
    ]
}

fn local(text: &str) -> LocalDateTime {
    text.parse().unwrap()
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
async fn events_snapshot_keeps_masters_and_singles_and_drops_occurrences() {
    // The fixture delta carries a master + 2 singles + 2 occurrences + 1 exception. The
    // master and singles are stored; the occurrences are dropped (the engine re-expands
    // the master) and the exception becomes an override of its series, not an object.
    let client = fake_client(delta_routes());
    let EventsPage { page, overrides } = events_page(
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
    // One edited occurrence, plus the two the master says were removed.
    assert_eq!(overrides.len(), 3);
    // The pass ends at the deltaLink, which becomes the persisted cursor.
    assert!(page.next_cursor.as_str().contains("deltatoken"));
}

#[tokio::test]
async fn a_pass_folds_the_edited_and_the_removed_occurrences_onto_the_series() {
    let client = fake_client(delta_routes());
    let EventsPage { page, overrides } = events_page(
        &client,
        &calendar_id(),
        None,
        None,
        window(),
        "Europe/Amsterdam",
    )
    .await
    .unwrap();

    let mut changed = page.changed;
    cal_override::fold_into(&mut changed, overrides);
    let master = changed
        .iter()
        .find(|e| e.is_recurring())
        .expect("the series master");
    let folded = &master.recurrence.as_ref().unwrap().overrides;

    // The standup runs Mondays at 09:00, so every key is that series' recurrence id.
    assert!(
        matches!(
            folded[&local("2026-08-10T09:00:00")],
            RecurrenceOverride::Patch(_)
        ),
        "the 10th was moved to 11:00"
    );
    for gone in ["2026-08-17T09:00:00", "2026-08-31T09:00:00"] {
        assert!(
            matches!(folded[&local(gone)], RecurrenceOverride::Excluded),
            "{gone} was removed and must stop being drawn"
        );
    }
    assert_eq!(folded.len(), 3);
}

#[tokio::test]
async fn a_series_master_is_re_read_in_its_own_zone_not_the_display_zone() {
    // Graph names an occurrence by its date in the zone the series was **authored** in,
    // and that name does not follow `Prefer: outlook.timezone` while `start` does. Asking
    // for the master in the display zone would therefore key every override on whatever
    // day the display zone happens to put it — one day out for any series near midnight.
    let (client, prefers) = fake_client_recording(
        delta_routes()
            .into_iter()
            .map(|(key, doc)| (key, Ok(doc)))
            .collect(),
    );
    events_page(
        &client,
        &calendar_id(),
        None,
        None,
        window(),
        "Pacific/Auckland",
    )
    .await
    .unwrap();

    let asked = prefers.lock().unwrap().clone();
    let (_, delta_prefer) = asked
        .iter()
        .find(|(url, _)| url.contains("calendarView/delta"))
        .expect("the delta was fetched");
    assert_eq!(
        delta_prefer.as_deref(),
        Some("outlook.timezone=\"Pacific/Auckland\""),
        "the pass itself still reads in the host's display zone"
    );
    let (_, master_prefer) = asked
        .iter()
        .find(|(url, _)| url.contains("cancelledOccurrences"))
        .expect("the master was re-read");
    assert_eq!(
        master_prefer.as_deref(),
        Some("outlook.timezone=\"W. Europe Standard Time\""),
        "the master is read in its own originalStartTimeZone"
    );
}

#[tokio::test]
async fn a_master_without_an_authoring_zone_keeps_the_display_zone() {
    // Nothing to disagree with, so the header the rest of the pass uses stands.
    let mut delta = json(DELTA);
    delta["value"][0]
        .as_object_mut()
        .unwrap()
        .remove("originalStartTimeZone");
    let (client, prefers) = fake_client_recording(vec![
        ("calendarView/delta", Ok(delta)),
        ("$select=start,end,cancelledOccurrences", Ok(json(MASTER))),
    ]);
    events_page(
        &client,
        &calendar_id(),
        None,
        None,
        window(),
        "Pacific/Auckland",
    )
    .await
    .unwrap();

    let asked = prefers.lock().unwrap().clone();
    let (_, master_prefer) = asked
        .iter()
        .find(|(url, _)| url.contains("cancelledOccurrences"))
        .expect("the master was re-read");
    assert_eq!(
        master_prefer.as_deref(),
        Some("outlook.timezone=\"Pacific/Auckland\"")
    );
}

#[tokio::test]
async fn the_re_read_start_replaces_the_delta_entry_s_own() {
    // The whole point of the second request is the zone it answers in, so what it says
    // has to reach the stored event — including the raw payload kept beside it.
    let mut delta = json(DELTA);
    delta["value"][0]["start"] = sjson!({
        "dateTime": "2026-08-03T18:00:00.0000000",
        "timeZone": "Pacific/Auckland"
    });
    let client = fake_client(vec![
        ("calendarView/delta", delta),
        ("$select=start,end,cancelledOccurrences", json(MASTER)),
    ]);
    let EventsPage { page, .. } = events_page(
        &client,
        &calendar_id(),
        None,
        None,
        window(),
        "Pacific/Auckland",
    )
    .await
    .unwrap();

    let master = page
        .changed
        .iter()
        .find(|e| e.is_recurring())
        .expect("the series master");
    let engine_core::time::CalendarDateTime::Zoned { local: at, zone } = &master.start else {
        panic!("a timed series master");
    };
    assert_eq!(at.to_string(), "2026-08-03T09:00:00");
    // Graph reports the authoring zone as a Windows name, whose CLDR mapping is that
    // zone's representative IANA name — the same offsets and the same DST rules.
    assert_eq!(zone.as_str(), "Europe/Berlin");
}

#[tokio::test]
async fn a_delta_tombstones_a_removed_event() {
    let cursor = SyncState::new("https://graph.test/me/calendars/cal-1/calendarView/delta?token=1");
    let removed = sjson!({
        "@odata.deltaLink": "https://graph.test/me/calendarView/delta?$deltatoken=next",
        "value": [ { "id": "evt-gone", "@removed": { "reason": "deleted" } } ]
    });
    let client = fake_client(vec![("token=1", removed)]);
    let EventsPage { page, overrides } = events_page(
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
    assert!(overrides.is_empty());
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

#[tokio::test]
async fn a_failed_master_re_read_fails_the_page() {
    // Silently keeping the delta's own reading would store the series in the wrong zone
    // and drop every removal on it, with nothing to say so.
    let client = fake_client_fallible(vec![
        ("calendarView/delta", Ok(json(DELTA))),
        (
            "$select=start,end,cancelledOccurrences",
            Err((503, sjson!({ "error": { "code": "ServiceUnavailable" } }))),
        ),
    ]);
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
