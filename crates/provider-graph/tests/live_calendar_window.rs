//! Gated live proof of what a Graph `calendarView/delta` cursor covers, the fact
//! `engine_sync::reach_calendar` is built on.
//!
//! A `deltaLink` reports changes **only inside the window it was opened over**: an event created
//! past that window never arrives through it, however long the cursor runs. Asserting that
//! absence alone would prove nothing (a delta that reported nothing at all would pass), so the
//! same delta must carry the control: an event created *inside* the window, at the same moment.
//! Then the remedy: reopening the cursor over the union of the window and the further range
//! lists the outside event, and a change to it afterwards arrives through the reopened cursor,
//! which is what keeps a reached range current.

mod common;

use std::time::Duration;

use common::*;
use engine_core::{
    calendar::Event,
    ids::{CalendarId, EventId, Uid},
    sync::{SyncState, SyncUpdate},
    time::Horizon,
};
use engine_provider::{
    CalendarSync, CalendarWrites, EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget,
};

/// How long a delta is polled for a change Graph has accepted but not yet reported.
const SETTLE: Duration = Duration::from_secs(45);

#[tokio::test]
async fn live_a_delta_reports_only_inside_its_window_and_a_reopened_cursor_covers_the_union() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_window: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = graph_calendars(&token);
    let calendar = default_calendar(&provider).await;

    // Far enough ahead that no other suite writes there: a week opened as the window, and a
    // day two months past it.
    let today = time::OffsetDateTime::now_utc().date();
    let inside_day = today + time::Duration::days(300);
    let outside_day = today + time::Duration::days(360);
    let window = days(
        inside_day - time::Duration::days(1),
        inside_day + time::Duration::days(6),
    );
    let further = days(outside_day, outside_day + time::Duration::days(1));

    let opened = provider
        .sync_events(&account(), &calendar, None, window)
        .await
        .expect("open the cursor over the window");
    assert!(opened.is_snapshot());

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let inside = create(&provider, &calendar, &format!("in-{unique}"), inside_day).await;
    let outside = create(&provider, &calendar, &format!("out-{unique}"), outside_day).await;

    // The control arm first: the delta does report a creation inside its window.
    let (seen, cursor) = delta_until(&provider, &calendar, &opened.next_cursor, &inside).await;
    assert!(
        seen.contains(&inside),
        "the delta reports an event inside its window"
    );
    // Then the cursor is given the whole settle period to report the outside one too.
    let (later, _) = delta_until(&provider, &calendar, &cursor, &outside).await;
    assert!(
        !seen.contains(&outside) && !later.contains(&outside),
        "the same cursor never reports an event created past its window"
    );

    // Reopened over the union, the outside event is listed, and its cursor follows it.
    let union = window.union(further);
    let reopened = provider
        .sync_events(&account(), &calendar, None, union)
        .await
        .expect("reopen over the union");
    let SyncUpdate::Snapshot { objects, .. } = &reopened.update else {
        panic!("a reopen is a snapshot");
    };
    let base = objects
        .iter()
        .find(|event| event.id == outside)
        .expect("the reopened cursor lists the event past the old window")
        .clone();
    assert!(objects.iter().any(|event| event.id == inside));

    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new("2026-07-18T10:05:00Z".parse().unwrap())
            .summary("provider-graph live window probe (moved on)"),
    );
    provider
        .patch_event(&account(), &base, &edit)
        .await
        .expect("patch the outside event");
    let (seen, _) = delta_until(&provider, &calendar, &reopened.next_cursor, &outside).await;
    assert!(
        seen.contains(&outside),
        "a change to the reached event arrives through the reopened cursor"
    );

    for event in objects.iter().filter(|e| e.id == inside || e.id == outside) {
        let _ = provider
            .delete_event(
                &account(),
                None,
                &EventDeletion::of(&latest(&provider, &calendar, union, &event.id).await),
            )
            .await;
    }
}

/// The account's default calendar.
async fn default_calendar(provider: &provider_graph::GraphCalendarProvider) -> CalendarId {
    let listing = provider
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars")
        .sync;
    let SyncUpdate::Snapshot { objects, .. } = listing.update else {
        panic!("expected a calendar snapshot");
    };
    objects
        .into_iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
        .id
}

/// `[from, to)` as a horizon of whole UTC days.
fn days(from: time::Date, to: time::Date) -> Horizon {
    Horizon::new(
        format!("{from}T00:00:00Z").parse().unwrap(),
        format!("{to}T00:00:00Z").parse().unwrap(),
    )
    .unwrap()
}

/// Creates a half-hour event on `day`, returning its id.
async fn create(
    provider: &provider_graph::GraphCalendarProvider,
    calendar: &CalendarId,
    name: &str,
    day: time::Date,
) -> EventId {
    let draft = EventDraft::new(
        calendar.clone(),
        Uid::new(format!("live-window-{name}@allodia-e2e.test")).unwrap(),
        "provider-graph live window probe",
        zoned(&format!("{day}T10:00:00")),
        zoned(&format!("{day}T10:30:00")),
        "2026-07-18T10:00:00Z".parse().unwrap(),
    )
    .description("safe to delete");
    provider
        .create_event(&account(), &draft)
        .await
        .expect("create_event")
        .event
}

/// Polls deltas from `cursor` until `want` appears or [`SETTLE`] runs out, returning every
/// event id the deltas reported and the cursor they ended on.
async fn delta_until(
    provider: &provider_graph::GraphCalendarProvider,
    calendar: &CalendarId,
    cursor: &SyncState,
    want: &EventId,
) -> (Vec<EventId>, SyncState) {
    let deadline = std::time::Instant::now() + SETTLE;
    let mut cursor = cursor.clone();
    let mut seen = Vec::new();
    loop {
        let delta = provider
            .sync_events(&account(), calendar, Some(&cursor), days_never())
            .await
            .expect("delta");
        assert!(!delta.is_snapshot(), "a live cursor answers a delta");
        if let SyncUpdate::Delta { changed, .. } = &delta.update {
            seen.extend(changed.iter().map(|event| event.id.clone()));
        }
        cursor = delta.next_cursor;
        if seen.contains(want) || std::time::Instant::now() > deadline {
            return (seen, cursor);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// A window no delta may use: a delta keeps the window its cursor was opened over, so passing
/// this one proves nothing reads it.
fn days_never() -> Horizon {
    days(
        time::Date::from_calendar_date(2001, time::Month::January, 1).unwrap(),
        time::Date::from_calendar_date(2001, time::Month::January, 2).unwrap(),
    )
}

/// The event as the server now has it, so a delete carries its current `ETag`.
async fn latest(
    provider: &provider_graph::GraphCalendarProvider,
    calendar: &CalendarId,
    window: Horizon,
    id: &EventId,
) -> Event {
    let snapshot = provider
        .sync_events(&account(), calendar, None, window)
        .await
        .expect("read back for cleanup");
    let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
        panic!("expected a snapshot");
    };
    objects
        .into_iter()
        .find(|event| &event.id == id)
        .expect("the probe is still there")
}
