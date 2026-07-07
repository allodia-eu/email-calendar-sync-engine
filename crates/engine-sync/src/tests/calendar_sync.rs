//! Calendar-sync loop tests over a real store: containers, events, and
//! materialized occurrence rows, plus an unsupported recurrence stored without
//! occurrences rather than failing the whole sync. Uses the shared fakes and
//! helpers from the parent module via `use super::*`.

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
    assert_eq!(report.events.upserted, 2);

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
    assert_eq!(report.events.upserted, 1);

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
