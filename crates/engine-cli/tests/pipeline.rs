//! End-to-end pipeline: ingest fixtures, search, and run the maintenance
//! (horizon-advance / re-expansion) path through the CLI library.

use engine_cli::{
    Fixture, Horizon, ingest, open_in_memory, reexpand_calendar, search_calendar, search_mail,
};
use engine_core::calendar::{Event, Frequency, Recurrence, RecurrenceRule};
use engine_core::ids::{AccountId, CalendarId, EventId, MailboxId, MessageId, Uid};
use engine_core::mail::{EmailAddress, Message};
use engine_core::membership::Memberships;
use engine_core::time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime};

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn utc_zone() -> TimeZoneId {
    TimeZoneId::utc()
}

fn instant(text: &str) -> UtcDateTime {
    text.parse().unwrap()
}

fn horizon(start: &str, end: &str) -> Horizon {
    Horizon::new(instant(start), instant(end)).unwrap()
}

/// A daily, unbounded standup at 09:00 UTC.
fn daily_standup() -> Event {
    let mut event = Event::new(
        EventId::try_from("daily").unwrap(),
        Uid::new("u-daily").unwrap(),
        Memberships::of_one(CalendarId::try_from("cal").unwrap()),
        CalendarDateTime::utc(LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap()),
    );
    event.title = String::from("Standup");
    event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));
    event
}

fn message(id: &str, subject: &str, from: &str) -> Message {
    let mut m = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    );
    m.envelope.subject = Some(subject.to_owned());
    m.envelope.from = vec![EmailAddress::new(from)];
    m
}

#[tokio::test]
async fn recurring_calendar_ingest_search_and_horizon_advance() {
    let store = open_in_memory().unwrap();
    let fixture = Fixture {
        events: vec![daily_standup()],
        messages: Vec::new(),
    };

    // Ingest with a 3-day horizon: only 3 occurrences materialize.
    let small = horizon("2026-06-01T00:00:00Z", "2026-06-04T00:00:00Z");
    let report = ingest(&store, account(), &fixture, &small, &utc_zone())
        .await
        .unwrap();
    assert_eq!(report.events, 1);
    assert_eq!(report.occurrences, 3);

    // A range inside the materialized window finds the event...
    let inside = search_calendar(&store, account(), "after:2026-06-01 before:2026-06-04", 10)
        .await
        .unwrap();
    assert_eq!(inside.keys().len(), 1);
    assert_eq!(inside.keys()[0].as_str(), "daily");

    // ...a range past the horizon does not yet (nothing materialized there).
    let beyond = search_calendar(&store, account(), "after:2026-06-10 before:2026-06-12", 10)
        .await
        .unwrap();
    assert!(beyond.hits.is_empty());

    // Advancing the horizon materializes further out through apply_maintenance.
    let wide = horizon("2026-06-01T00:00:00Z", "2026-06-20T00:00:00Z");
    let occurrences = reexpand_calendar(&store, account(), &wide, &utc_zone())
        .await
        .unwrap();
    assert_eq!(occurrences, 19); // 2026-06-01 .. 2026-06-19 inclusive

    // Now the previously-empty range matches.
    let now_found = search_calendar(&store, account(), "after:2026-06-10 before:2026-06-12", 10)
        .await
        .unwrap();
    assert_eq!(now_found.keys().len(), 1);
    assert_eq!(now_found.keys()[0].as_str(), "daily");
}

#[tokio::test]
async fn reexpansion_is_byte_stable_when_nothing_changes() {
    let store = open_in_memory().unwrap();
    let fixture = Fixture {
        events: vec![daily_standup()],
        messages: Vec::new(),
    };
    let window = horizon("2026-06-01T00:00:00Z", "2026-06-10T00:00:00Z");
    ingest(&store, account(), &fixture, &window, &utc_zone())
        .await
        .unwrap();

    // Re-expanding over the same horizon (a tzdata-bump with unchanged zones)
    // produces the same occurrence set and leaves the range answer identical.
    let first = search_calendar(&store, account(), "after:2026-06-01 before:2026-06-10", 50)
        .await
        .unwrap();
    let again = reexpand_calendar(&store, account(), &window, &utc_zone())
        .await
        .unwrap();
    assert_eq!(again, 9);
    let second = search_calendar(&store, account(), "after:2026-06-01 before:2026-06-10", 50)
        .await
        .unwrap();
    assert_eq!(first.keys(), second.keys());
}

#[tokio::test]
async fn mail_ingest_and_search() {
    let store = open_in_memory().unwrap();
    let fixture = Fixture {
        events: Vec::new(),
        messages: vec![
            message("m1", "Lunch with Alice", "alice@example.com"),
            message("m2", "Project update", "bob@example.com"),
        ],
    };
    let report = ingest(&store, account(), &fixture, &horizon_default(), &utc_zone())
        .await
        .unwrap();
    assert_eq!(report.messages, 2);
    assert_eq!(report.occurrences, 0);

    let by_from = search_mail(&store, account(), "from:alice@example.com", 10)
        .await
        .unwrap();
    assert_eq!(by_from.keys().len(), 1);
    assert_eq!(by_from.keys()[0].as_str(), "m1");

    let by_subject = search_mail(&store, account(), "subject:lunch", 10)
        .await
        .unwrap();
    assert_eq!(by_subject.keys().len(), 1);
    assert_eq!(by_subject.keys()[0].as_str(), "m1");
}

#[tokio::test]
async fn fixture_json_roundtrips_and_ingests() {
    // The fixture format is JSON of normalized engine-core objects.
    let json = serde_json::to_string(&serde_json::json!({
        "events": [daily_standup()],
        "messages": [message("m1", "Hello", "a@example.com")],
    }))
    .unwrap();
    let fixture = Fixture::from_json(&json).unwrap();
    assert_eq!(fixture.events.len(), 1);
    assert_eq!(fixture.messages.len(), 1);

    let store = open_in_memory().unwrap();
    let report = ingest(&store, account(), &fixture, &horizon_default(), &utc_zone())
        .await
        .unwrap();
    assert_eq!(report.events, 1);
    assert_eq!(report.messages, 1);
}

fn horizon_default() -> Horizon {
    horizon("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z")
}
