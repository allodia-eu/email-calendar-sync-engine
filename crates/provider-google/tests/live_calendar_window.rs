//! Gated live measurement of what a Google `syncToken` taken after a **bounded** `events.list`
//! covers, and proof of the reopen `engine_sync::reach_calendar` relies on.
//!
//! Google forbids `timeMin`/`timeMax` beside a `syncToken` and does not document whether the
//! token then reports changes past the bounds of the list it ended. Measured: **it does**, unlike
//! a Graph `deltaLink`. That still leaves the events that were there before the bounded list and
//! never changed, which only a list over their dates fetches. So this asks it: open a
//! cursor over a week, create one event inside the week and one two months past it at the same
//! moment, and give the cursor the settle period to report each. The inside event is the control
//! arm; without it, a cursor that reported nothing at all would pass for one that ignores the
//! outside event. Then the remedy: a cursor reopened over the union lists the outside event, and
//! a change to it arrives through that cursor.

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
use provider_google::GoogleCalendarProvider;

/// How long a delta is polled for a change Google has accepted but not yet reported.
const SETTLE: Duration = Duration::from_secs(45);

#[tokio::test]
async fn live_a_bounded_sync_token_and_a_reopened_cursor() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_window: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = google_calendars(token);
    let calendar = primary(&provider).await;

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

    let (seen, cursor) = delta_until(&provider, &calendar, &opened.next_cursor, &inside).await;
    assert!(
        seen.contains(&inside),
        "the token reports an event inside the bounds of the list it ended"
    );
    let (later, _) = delta_until(&provider, &calendar, &cursor, &outside).await;
    // Measured: it does. Graph's `deltaLink` does not
    // (`provider-graph/tests/live_calendar_window.rs`), so the engine reopens rather than
    // relying on either.
    assert!(
        seen.contains(&outside) || later.contains(&outside),
        "a bounded syncToken reports an event created past the bounds of the list it ended"
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
            .summary("provider-google live window probe (moved on)"),
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

    for id in [&inside, &outside] {
        let event = latest(&provider, &calendar, union, id).await;
        let _ = provider
            .delete_event(&account(), Some(&event), &EventDeletion::of(&event))
            .await;
    }
}

/// `[from, to)` as a horizon of whole UTC days.
fn days(from: time::Date, to: time::Date) -> Horizon {
    Horizon::new(
        format!("{from}T00:00:00Z").parse().unwrap(),
        format!("{to}T00:00:00Z").parse().unwrap(),
    )
    .unwrap()
}

/// Creates an hour-long event on `day`, returning its id.
async fn create(
    provider: &GoogleCalendarProvider,
    calendar: &CalendarId,
    name: &str,
    day: time::Date,
) -> EventId {
    let draft = EventDraft::new(
        calendar.clone(),
        Uid::new(format!("live-window-{name}@example.test")).unwrap(),
        "provider-google live window probe",
        zoned(&format!("{day}T10:00:00")),
        zoned(&format!("{day}T11:00:00")),
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
    provider: &GoogleCalendarProvider,
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

/// A window no delta may use: a delta continues the window its cursor was opened over, so
/// passing this one proves nothing reads it.
fn days_never() -> Horizon {
    days(
        time::Date::from_calendar_date(2001, time::Month::January, 1).unwrap(),
        time::Date::from_calendar_date(2001, time::Month::January, 2).unwrap(),
    )
}

/// The event as the server now has it, so a delete carries its current `ETag`.
async fn latest(
    provider: &GoogleCalendarProvider,
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
