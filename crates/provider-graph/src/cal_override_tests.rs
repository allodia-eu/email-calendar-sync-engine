//! Offline tests for reading Graph's occurrence-level entries and folding them onto the
//! series they name.

use engine_core::{
    calendar::{Event, Frequency, RecurrenceRule},
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
};
use serde_json::json as sjson;

use super::*;

const MASTER: &str = "series-master-1";

fn local(text: &str) -> LocalDateTime {
    text.parse().unwrap()
}

fn master_key() -> ProviderKey {
    ProviderKey::new(MASTER).unwrap()
}

/// A weekly series starting 09:00 Amsterdam, the shape every fold below keys against.
fn weekly_master() -> Event {
    let mut event = Event::new(
        EventId::try_from(MASTER).unwrap(),
        Uid::new("series@example.test").unwrap(),
        Memberships::of_one(CalendarId::try_from("cal-1").unwrap()),
        CalendarDateTime::Zoned {
            local: local("2026-08-03T09:00:00"),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        },
    );
    let mut recurrence = Recurrence::default();
    recurrence
        .rules
        .push(RecurrenceRule::new(Frequency::Weekly));
    event.recurrence = Some(recurrence);
    event
}

/// One `type: "exception"` delta entry: the 10th moved to 11:00 and renamed.
fn exception_entry() -> Value {
    sjson!({
        "id": "opaque-after-the-patch",
        "type": "exception",
        "seriesMasterId": MASTER,
        "occurrenceId": format!("OID.{MASTER}.2026-08-10"),
        "subject": "Standup (moved to 11:00)",
        "start": { "dateTime": "2026-08-10T11:00:00.0000000", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-08-10T11:30:00.0000000", "timeZone": "Europe/Amsterdam" },
    })
}

#[test]
fn an_exception_keys_on_the_date_it_came_from_not_the_one_it_moved_to() {
    // Graph keeps the derived `OID.<master>.<date>` in `occurrenceId` even after the patch
    // made `id` opaque, and that date is the occurrence's **original** one. Keying off
    // where it landed would leave the original slot drawn and add a stray instance.
    let mut entry = exception_entry();
    entry["start"] = sjson!({
        "dateTime": "2026-08-09T20:00:00.0000000",
        "timeZone": "Europe/Amsterdam"
    });
    entry["end"] = sjson!({
        "dateTime": "2026-08-09T20:30:00.0000000",
        "timeZone": "Europe/Amsterdam"
    });

    let mut events = [weekly_master()];
    fold_into(&mut events, vec![pending_override(&entry).unwrap()]);

    let overrides = &events[0].recurrence.as_ref().unwrap().overrides;
    assert_eq!(overrides.len(), 1);
    assert!(
        overrides.contains_key(&local("2026-08-10T09:00:00")),
        "keyed at the original recurrence id, not the moved start: {:?}",
        overrides.keys().collect::<Vec<_>>()
    );
}

#[test]
fn an_exception_carries_where_it_moved_to_and_what_it_is_called() {
    let mut events = [weekly_master()];
    fold_into(
        &mut events,
        vec![pending_override(&exception_entry()).unwrap()],
    );

    let overrides = &events[0].recurrence.as_ref().unwrap().overrides;
    let RecurrenceOverride::Patch(patch) = &overrides[&local("2026-08-10T09:00:00")] else {
        panic!("expected a patch");
    };
    assert_eq!(
        patch.get("start").and_then(Value::as_str),
        Some("2026-08-10T11:00:00")
    );
    assert_eq!(
        patch.get("timeZone").and_then(Value::as_str),
        Some("Europe/Amsterdam")
    );
    assert_eq!(patch.get("duration").and_then(Value::as_str), Some("PT30M"));
    assert_eq!(
        patch.get("title").and_then(Value::as_str),
        Some("Standup (moved to 11:00)")
    );
    // Nothing was cancelled, so the patch says nothing about status.
    assert!(patch.get("status").is_none());
}

#[test]
fn an_exception_carries_its_own_notes_and_room() {
    // Graph states the whole instance, so a note or a room the user changed for one
    // occurrence is right there on the entry — and used to be thrown away.
    let mut entry = exception_entry();
    entry["body"] = sjson!({ "contentType": "text", "content": "Bring the printout" });
    entry["location"] = sjson!({ "displayName": "Room 2" });

    let mut events = [weekly_master()];
    fold_into(&mut events, vec![pending_override(&entry).unwrap()]);

    let overrides = &events[0].recurrence.as_ref().unwrap().overrides;
    let RecurrenceOverride::Patch(patch) = &overrides[&local("2026-08-10T09:00:00")] else {
        panic!("expected a patch");
    };
    assert_eq!(
        patch.get("description").and_then(Value::as_str),
        Some("Bring the printout")
    );
    let room = patch
        .get("locations")
        .and_then(Value::as_object)
        .and_then(|map| map.values().next())
        .expect("a locations map");
    assert_eq!(room.get("name").and_then(Value::as_str), Some("Room 2"));
}

#[test]
fn an_organizer_cancelled_occurrence_is_marked_cancelled_in_its_patch() {
    // Graph's `isCancelled` is the organizer calling one occurrence off, which iCalendar
    // states as `STATUS:CANCELLED` on the override — not the same thing as the attendee
    // removing it, which arrives in `cancelledOccurrences` instead.
    let mut entry = exception_entry();
    entry["isCancelled"] = sjson!(true);

    let mut events = [weekly_master()];
    fold_into(&mut events, vec![pending_override(&entry).unwrap()]);

    let overrides = &events[0].recurrence.as_ref().unwrap().overrides;
    let RecurrenceOverride::Patch(patch) = &overrides[&local("2026-08-10T09:00:00")] else {
        panic!("expected a patch");
    };
    assert_eq!(
        patch.get("status").and_then(Value::as_str),
        Some("cancelled")
    );
}

#[test]
fn every_cancelled_occurrence_of_a_master_becomes_an_exclusion() {
    let doc = sjson!({
        "id": MASTER,
        "cancelledOccurrences": [
            format!("OID.{MASTER}.2026-08-17"),
            format!("OID.{MASTER}.2026-08-31"),
        ]
    });
    let mut events = [weekly_master()];
    fold_into(&mut events, cancellations(&master_key(), &doc).unwrap());

    let overrides = &events[0].recurrence.as_ref().unwrap().overrides;
    assert_eq!(overrides.len(), 2, "the whole array is read, not its head");
    for at in ["2026-08-17T09:00:00", "2026-08-31T09:00:00"] {
        assert!(
            matches!(overrides[&local(at)], RecurrenceOverride::Excluded),
            "{at} should be excluded"
        );
    }
}

#[test]
fn a_master_with_no_cancellations_yields_none() {
    let doc = sjson!({ "id": MASTER });
    assert!(cancellations(&master_key(), &doc).unwrap().is_empty());
}

#[test]
fn the_date_is_the_last_segment_because_a_graph_id_carries_dots() {
    // A real `seriesMasterId` is base64url-ish and contains `.`, so splitting forwards on
    // the third segment reads part of the id as a date and the entry is silently lost.
    let dotted = "AQMkAD.AwATNiZmYA.ZS1iNDJl";
    let doc = sjson!({ "cancelledOccurrences": [format!("OID.{dotted}.2026-08-17")] });
    let pending = cancellations(&ProviderKey::new(dotted).unwrap(), &doc).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].on, CalendarDate::new(2026, 8, 17).unwrap());
}

#[test]
fn an_all_day_series_keys_at_midnight() {
    let mut event = weekly_master();
    event.start = CalendarDateTime::Date(CalendarDate::new(2026, 8, 3).unwrap());
    let doc = sjson!({ "cancelledOccurrences": [format!("OID.{MASTER}.2026-08-17")] });

    let mut events = [event];
    fold_into(&mut events, cancellations(&master_key(), &doc).unwrap());

    let overrides = &events[0].recurrence.as_ref().unwrap().overrides;
    assert!(overrides.contains_key(&local("2026-08-17T00:00:00")));
}

#[test]
fn an_override_whose_master_is_not_in_the_pass_is_dropped() {
    let doc = sjson!({ "cancelledOccurrences": [format!("OID.{MASTER}.2026-08-17")] });
    let stranger = ProviderKey::new("some-other-series").unwrap();

    let mut events = [weekly_master()];
    fold_into(&mut events, cancellations(&stranger, &doc).unwrap());

    assert!(
        events[0].recurrence.as_ref().unwrap().overrides.is_empty(),
        "an override may only reach the series it names"
    );
}

#[test]
fn an_entry_without_a_series_master_is_a_protocol_error() {
    let mut entry = exception_entry();
    entry.as_object_mut().unwrap().remove("seriesMasterId");
    assert!(pending_override(&entry).is_err());
}

#[test]
fn an_occurrence_id_with_an_unparseable_date_is_a_protocol_error() {
    let doc = sjson!({ "cancelledOccurrences": [format!("OID.{MASTER}.not-a-date")] });
    assert!(cancellations(&master_key(), &doc).is_err());
}
