//! What a `recurringEventId` entry becomes, and which series it lands on.
//!
//! The read side of a Google series is assembled from entries that arrive separately and in
//! no order, so these tests state both halves: the shape one entry folds into, and the
//! correlation that puts it on the right event.

use engine_core::{
    calendar::{Frequency, RecurrenceRule},
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{LocalDateTime, TimeZoneId},
};
use serde_json::json;

use super::*;

const MASTER: &str = "evt-1";

fn calendar() -> CalendarId {
    CalendarId::try_from("primary").unwrap()
}

fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// A weekly series, as the normalizer hands it over.
fn series() -> Event {
    let mut event = Event::new(
        EventId::try_from(MASTER).unwrap(),
        Uid::new("evt-1@google.com").unwrap(),
        Memberships::of_one(calendar()),
        zoned("2026-09-07T09:30:00"),
    );
    event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(
        Frequency::Weekly,
    )));
    event
}

/// An entry for an occurrence somebody moved and renamed.
fn moved() -> Value {
    json!({
        "id": "evt-1_20260914T073000Z",
        "recurringEventId": MASTER,
        "originalStartTime": { "dateTime": "2026-09-14T09:30:00+02:00",
                               "timeZone": "Europe/Amsterdam" },
        "start": { "dateTime": "2026-09-14T14:00:00+02:00", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-09-14T14:45:00+02:00", "timeZone": "Europe/Amsterdam" },
        "summary": "Moved to the afternoon",
        "status": "confirmed",
    })
}

/// An entry for an occurrence somebody deleted.
fn cancelled() -> Value {
    json!({
        "id": "evt-1_20260921T073000Z",
        "recurringEventId": MASTER,
        "originalStartTime": { "dateTime": "2026-09-21T09:30:00+02:00",
                               "timeZone": "Europe/Amsterdam" },
        "status": "cancelled",
    })
}

fn overrides_of(event: &Event) -> BTreeMap<LocalDateTime, RecurrenceOverride> {
    event
        .recurrence
        .as_ref()
        .expect("a series")
        .overrides
        .clone()
}

#[test]
fn a_moved_occurrence_keeps_its_original_start_as_its_key() {
    // The key is the occurrence's identity within the series, so it is `originalStartTime`
    // and never the time it was moved to — keying by the new start would override an
    // instance the rule does not produce, and leave the one the user moved still drawn.
    let mut events = vec![series()];
    fold_into(&mut events, vec![pending_override(&moved(), None).unwrap()]);

    let overrides = overrides_of(&events[0]);
    let RecurrenceOverride::Patch(patch) = overrides
        .get(&"2026-09-14T09:30:00".parse().unwrap())
        .expect("keyed by the original start")
    else {
        panic!("a moved occurrence is a patch, not an exclusion");
    };
    assert_eq!(patch.get("start").unwrap(), "2026-09-14T14:00:00");
    assert_eq!(patch.get("timeZone").unwrap(), "Europe/Amsterdam");
    assert_eq!(patch.get("duration").unwrap(), "PT45M");
    assert_eq!(patch.get("title").unwrap(), "Moved to the afternoon");
}

#[test]
fn a_changed_occurrence_carries_its_own_notes_and_room() {
    // Google states the whole instance, so the note and the room the user changed for this
    // one occurrence are on the entry — and used to be dropped, leaving it showing the
    // series' own.
    let mut entry = moved();
    entry["description"] = json!("Bring the printout");
    entry["location"] = json!("Room 2");

    let mut events = vec![series()];
    fold_into(&mut events, vec![pending_override(&entry, None).unwrap()]);

    let overrides = overrides_of(&events[0]);
    let RecurrenceOverride::Patch(patch) = overrides
        .get(&"2026-09-14T09:30:00".parse().unwrap())
        .expect("keyed by the original start")
    else {
        panic!("expected a patch");
    };
    assert_eq!(patch.get("description").unwrap(), "Bring the printout");
    // JSCalendar has no scalar location, so one piece of text still projects as a map —
    // the shape the JMAP reader passes through for the same occurrence.
    let room = patch
        .get("locations")
        .and_then(Value::as_object)
        .and_then(|map| map.values().next())
        .expect("a locations map");
    assert_eq!(room.get("name").unwrap(), "Room 2");
}

#[test]
fn a_cancelled_occurrence_is_an_exclusion_and_carries_nothing_else() {
    // RFC 8984 §4.3.3 makes that structural, and the entry has no `start` to read anyway.
    let mut events = vec![series()];
    fold_into(
        &mut events,
        vec![pending_override(&cancelled(), None).unwrap()],
    );

    assert_eq!(
        overrides_of(&events[0]).get(&"2026-09-21T09:30:00".parse().unwrap()),
        Some(&RecurrenceOverride::Excluded)
    );
}

#[test]
fn every_entry_lands_on_the_series_that_names_it() {
    // The correlation is by `recurringEventId`, not by position: the entries arrive in no
    // order, and a page can carry several series at once.
    let mut other = series();
    other.id = EventId::try_from("evt-2").unwrap();
    other.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));
    let mut events = vec![other, series()];

    fold_into(
        &mut events,
        vec![
            pending_override(&cancelled(), None).unwrap(),
            pending_override(&moved(), None).unwrap(),
        ],
    );

    assert_eq!(
        overrides_of(&events[0]).len(),
        0,
        "the other series is untouched"
    );
    assert_eq!(
        overrides_of(&events[1]).len(),
        2,
        "and both entries landed on the one they name"
    );
}

#[test]
fn an_entry_whose_series_is_not_in_the_pass_is_dropped() {
    // It cannot be applied: `Recurrence` lives inside the master, and the store takes whole
    // objects. Measured not to arise — a delta that changes an occurrence carries its master
    // — so this is the shape of the drop, not a path to build on.
    let mut events = vec![series()];
    let mut orphan = pending_override(&moved(), None).unwrap();
    orphan.master = ProviderKey::new("evt-elsewhere").unwrap();

    fold_into(&mut events, vec![orphan]);
    assert!(overrides_of(&events[0]).is_empty());
}

#[test]
fn an_entry_naming_no_occurrence_is_a_protocol_error() {
    // Without `originalStartTime` there is no key to file it under, and inventing one — the
    // instance's *current* start, say — would override an instance the rule never produced.
    let mut without = moved();
    without.as_object_mut().unwrap().remove("originalStartTime");
    assert!(pending_override(&without, None).is_err());
}
