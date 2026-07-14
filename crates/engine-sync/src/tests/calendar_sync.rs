//! Calendar-sync loop tests over a real store: containers, events, and
//! materialized occurrence rows, an unsupported recurrence stored without
//! occurrences rather than failing the whole sync, an event that *moves* replacing its
//! occurrence rows instead of ghosting at both instants, and the event-only delta a
//! completed write reconciles through. Uses the shared fakes and helpers from the parent
//! module via `use super::*`.

use super::*;

fn calendar(id: &str, name: &str) -> Calendar {
    Calendar::new(CalendarId::try_from(id).unwrap(), name)
}

fn event(id: &str, uid: &str, calendar: &str, start: CalendarDateTime) -> Event {
    Event::new(
        EventId::try_from(id).unwrap(),
        Uid::new(uid).unwrap(),
        Memberships::of_one(CalendarId::try_from(calendar).unwrap()),
        start,
    )
}

fn zoned(year: i32, month: u8, day: u8, hour: u8) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: LocalDateTime::new(year, month, day, hour, 0, 0).unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn year_horizon() -> Horizon {
    Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2026-12-31T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn sync_calendar_stores_containers_events_and_occurrences() {
    let single = event("evt-1", "uid-1@h", "work", zoned(2026, 3, 1, 9));
    let mut weekly = event("evt-2", "uid-2@h", "work", zoned(2026, 1, 5, 9));
    weekly.duration = "PT30M".parse().unwrap();
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(3).unwrap());
    weekly.recurrence = Some(Recurrence::from_rule(rule));

    let provider = FakeMail::new(vec![], vec![])
        .with_calendar(vec![calendar("work", "Work")], vec![single, weekly]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &host_zone,
    )
    .await
    .unwrap();
    assert_eq!(report.calendars.upserted, 1);
    assert_eq!(report.events.applied.upserted, 2);

    let event_scope = provider.event_scope(&account());
    // Every event materializes occurrences: the single one once, the weekly-count-3
    // three times.
    assert_eq!(
        store
            .index_row_counts(&event_scope, &key("evt-1"))
            .await
            .unwrap()
            .occurrences,
        1
    );
    assert_eq!(
        store
            .index_row_counts(&event_scope, &key("evt-2"))
            .await
            .unwrap()
            .occurrences,
        3
    );
}

#[tokio::test]
async fn a_moved_event_replaces_its_occurrence_rows_rather_than_ghosting() {
    // Occurrence rows are keyed by (scope, event, start, recurrence-id) and upserted, so
    // an event whose start moves would add a row rather than move one — leaving the event
    // rendered at BOTH instants on the grid, forever. The projection must clear the
    // event's derived rows before rewriting them. This is what makes a reconciled write
    // (and any remote move) actually visible as a move.
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let calendars = vec![calendar("work", "Work")];

    let before = FakeMail::new(vec![], vec![]).with_calendar(
        calendars.clone(),
        vec![event("evt-1", "uid-1@h", "work", zoned(2026, 3, 1, 9))],
    );
    sync_calendar(
        &before,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &host_zone,
    )
    .await
    .unwrap();

    // The same event, now an hour later — the server's copy after somebody moved it.
    let after = FakeMail::new(vec![], vec![]).with_calendar(
        calendars,
        vec![event("evt-1", "uid-1@h", "work", zoned(2026, 3, 1, 10))],
    );
    sync_calendar(
        &after,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &host_zone,
    )
    .await
    .unwrap();

    assert_eq!(
        store
            .index_row_counts(&after.event_scope(&account()), &key("evt-1"))
            .await
            .unwrap()
            .occurrences,
        1,
        "the moved event must occupy one instant, not two"
    );
}

#[tokio::test]
async fn reconcile_calendar_events_applies_the_event_delta_without_the_container_scope() {
    // The read-your-writes step: after a write the store still holds the pre-write event,
    // so the write's caller re-reads through the delta the sync path already uses. It is
    // the EVENT scope alone — an event write cannot change the calendar list — which this
    // proves by reconciling against a provider whose calendar fetch would fail outright.
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let before = FakeMail::new(vec![], vec![]).with_calendar(
        vec![calendar("work", "Work")],
        vec![event("evt-1", "uid-1@h", "work", zoned(2026, 3, 1, 9))],
    );
    sync_calendar(
        &before,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &host_zone,
    )
    .await
    .unwrap();

    // The server's copy of the event a write just changed. Reconciling must take *this*
    // one — the server's, reserialization and all — never the bytes the write sent.
    let mut moved = event("evt-1", "uid-1@h", "work", zoned(2026, 3, 1, 10));
    moved.title = "As the server stored it".to_owned();
    let after = FakeMail::new(vec![], vec![])
        .with_calendar(vec![calendar("work", "Work")], vec![moved])
        .failing(Fault::CalendarFetch);

    let report =
        reconcile_calendar_events(&after, &store, &account(), worker(), Duration::from_mins(1))
            .await
            .unwrap();
    assert_eq!(report.applied.upserted, 1);
    assert!(report.unexpandable.is_empty());

    let scope = after.event_scope(&account());
    let stored: Event = serde_json::from_value(
        store
            .object_payload(&scope, &key("evt-1"))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.title, "As the server stored it");
    assert_eq!(
        store
            .index_row_counts(&scope, &key("evt-1"))
            .await
            .unwrap()
            .occurrences,
        1
    );
}

#[tokio::test]
async fn unsupported_recurrence_stores_event_without_occurrences() {
    let mut weird = event("evt-x", "uid-x@h", "work", zoned(2026, 3, 1, 9));
    // A sub-daily frequency is outside the expander's supported subset.
    weird.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(
        Frequency::Hourly,
    )));
    let provider =
        FakeMail::new(vec![], vec![]).with_calendar(vec![calendar("work", "Work")], vec![weird]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &host_zone,
    )
    .await
    .unwrap();
    assert_eq!(report.events.applied.upserted, 1);

    // The event is stored and indexed, but materializes no occurrences (rather than
    // failing the whole sync).
    let event_scope = provider.event_scope(&account());
    let counts = store
        .index_row_counts(&event_scope, &key("evt-x"))
        .await
        .unwrap();
    assert_eq!(counts.occurrences, 0);
    assert!(counts.event_index >= 1);
}

#[tokio::test]
async fn a_sync_re_expands_a_changed_event_over_the_stores_window_not_the_callers() {
    // The trap the store-owned `ExpansionWindow` exists to close. Clearing a changed event's
    // occurrence rows is unwindowed (it must be, or a moved event ghosts), so if the
    // re-expansion used the *caller's* horizon the event would keep only the rows inside it
    // — silently losing every occurrence the host had already expanded, while every
    // UNCHANGED event kept theirs. A weekly meeting would vanish from next month's grid
    // because somebody renamed it.
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let mut weekly = event("evt-w", "uid-w@h", "work", zoned(2026, 1, 5, 9));
    weekly.duration = "PT30M".parse().unwrap();
    weekly.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(
        Frequency::Weekly,
    )));

    // The app syncs on a narrow (one-month) horizon, as a calendar app showing one month does.
    let january = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2026-02-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let before = FakeMail::new(vec![], vec![])
        .with_calendar(vec![calendar("work", "Work")], vec![weekly.clone()]);
    sync_calendar(
        &before,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        january,
        &zone,
    )
    .await
    .unwrap();

    // The user scrolls out to the whole year, so the host widens the horizon — the one call
    // that moves the window, and it re-expands every event to match.
    expand_calendar_horizon(
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &zone,
    )
    .await
    .unwrap();
    let scope = before.event_scope(&account());
    let expanded = store
        .index_row_counts(&scope, &key("evt-w"))
        .await
        .unwrap()
        .occurrences;
    assert!(
        expanded > 40,
        "the year is materialized: {expanded} occurrences"
    );

    // Now the event changes remotely and a ROUTINE sync runs on the app's usual narrow
    // horizon. The changed event must keep the year the host expanded.
    let mut renamed = weekly;
    renamed.title = "Renamed".to_owned();
    let after =
        FakeMail::new(vec![], vec![]).with_calendar(vec![calendar("work", "Work")], vec![renamed]);
    sync_calendar(
        &after,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        january,
        &zone,
    )
    .await
    .unwrap();

    assert_eq!(
        store
            .index_row_counts(&scope, &key("evt-w"))
            .await
            .unwrap()
            .occurrences,
        expanded,
        "a changed event must not lose the occurrences the host already expanded just \
         because the sync that touched it was handed a narrower horizon"
    );
}

#[tokio::test]
async fn a_reconcile_before_any_sync_is_an_error_not_an_empty_calendar() {
    // Reconciling an event scope that has never been expanded has no window to expand over.
    // Expanding nothing would store the events with ZERO occurrence rows *and* advance the
    // cursor, so the next sync would report no changes and the grid would stay empty
    // forever. Refuse instead.
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let provider = FakeMail::new(vec![], vec![]).with_calendar(
        vec![calendar("work", "Work")],
        vec![event("evt-1", "uid-1@h", "work", zoned(2026, 3, 1, 9))],
    );

    let err = reconcile_calendar_events(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, crate::SyncError::NoExpansionWindow),
        "got {err:?}"
    );
}
