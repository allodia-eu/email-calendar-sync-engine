//! Conformance tests for the calendar model.

use engine_core::calendar::{
    Alert, AlertAction, Calendar, CalendarAccess, Event, EventKind, EventStatus, Frequency,
    Location, ParticipantRole, ParticipationStatus, Recurrence, RecurrenceOverride, RecurrenceRule,
    RelativeTo, Trigger, VirtualLocation,
};
use engine_core::extended::ExtendedProperties;
use engine_core::ids::{CalendarId, EventId, Uid};
use engine_core::membership::Memberships;
use engine_core::patch::PatchObject;
use engine_core::time::{CalendarDate, CalendarDateTime, Duration, LocalDateTime, TimeZoneId};

fn calendars(ids: &[&str]) -> Memberships<CalendarId> {
    Memberships::new(ids.iter().map(|id| CalendarId::try_from(*id).unwrap())).unwrap()
}

fn zoned(zone: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: LocalDateTime::new(2021, 6, 1, 9, 0, 0).unwrap(),
        zone: TimeZoneId::iana(zone).unwrap(),
    }
}

fn event(id: &str, cals: &[&str]) -> Event {
    Event::new(
        EventId::try_from(id).unwrap(),
        Uid::new("uid-1").unwrap(),
        calendars(cals),
        zoned("Europe/Amsterdam"),
    )
}

#[test]
fn event_has_multiple_calendar_memberships() {
    let ev = event("evt-1", &["work", "shared"]);
    assert_eq!(ev.calendars.len().get(), 2);
    assert!(
        ev.calendars
            .contains(&CalendarId::try_from("work").unwrap())
    );
}

#[test]
fn event_predicates_cover_every_state() {
    let mut ev = event("evt-1", &["work"]);
    assert!(!ev.is_recurring());
    assert!(!ev.is_override_instance());
    assert!(!ev.is_all_day());
    assert!(!ev.is_cancelled());

    ev.status = EventStatus::Cancelled;
    assert!(ev.is_cancelled());

    ev.recurrence_id = Some(zoned("Europe/Amsterdam"));
    assert!(ev.is_override_instance());

    let mut all_day = event("evt-2", &["work"]);
    all_day.start = CalendarDateTime::Date(CalendarDate::new(2021, 6, 1).unwrap());
    assert!(all_day.is_all_day());
}

#[test]
fn recurring_event_with_overrides_and_exclusions() {
    let mut ev = event("evt-1", &["work"]);
    let mut recurrence = Recurrence::from_rule(RecurrenceRule::new(Frequency::Weekly));
    let excluded_id: LocalDateTime = "2021-06-08T09:00:00".parse().unwrap();
    let moved_id: LocalDateTime = "2021-06-15T09:00:00".parse().unwrap();
    recurrence
        .overrides
        .insert(excluded_id, RecurrenceOverride::Excluded);
    recurrence.overrides.insert(
        moved_id,
        RecurrenceOverride::Patch(
            PatchObject::new([("title".to_owned(), serde_json::json!("Moved"))]).unwrap(),
        ),
    );
    ev.recurrence = Some(recurrence);

    assert!(ev.is_recurring());
    let rec = ev.recurrence.as_ref().unwrap();
    assert!(rec.is_excluded(&excluded_id));
    assert!(!rec.is_excluded(&moved_id));
}

#[test]
fn event_with_participants_locations_and_alerts() {
    let mut ev = event("evt-1", &["work"]);
    let mut attendee = engine_core::calendar::Participant::attendee("guest@example.com");
    attendee.participation_status = ParticipationStatus::Accepted;
    assert!(attendee.has_role(&ParticipantRole::Attendee));
    ev.participants.push(attendee);

    ev.locations.push(Location::named("HQ"));
    ev.virtual_locations
        .push(VirtualLocation::new("https://meet.example/x"));
    ev.alerts.push(Alert::display(Trigger::before_start(
        "PT10M".parse::<Duration>().unwrap(),
    )));
    ev.alerts.push(Alert {
        trigger: Trigger::Absolute {
            when: "2021-06-01T08:45:00Z".parse().unwrap(),
        },
        action: AlertAction::Email,
        acknowledged: Some("2021-06-01T08:46:00Z".parse().unwrap()),
    });

    assert_eq!(ev.participants.len(), 1);
    assert_eq!(ev.alerts.len(), 2);
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
}

#[test]
fn event_kinds_and_payload_are_preserved() {
    let mut ev = event("evt-1", &["work"]);
    ev.kind = EventKind::OutOfOffice;
    ev.extended.set(
        "google/outOfOfficeProperties",
        serde_json::json!({ "autoDeclineMode": "declineAllConflictingInvitations" }),
    );
    let json = serde_json::to_string(&ev).unwrap();
    let back: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(back.kind, EventKind::OutOfOffice);
    assert!(back.extended.get("google/outOfOfficeProperties").is_some());

    // A provider event type the engine does not know is preserved verbatim.
    let unknown = EventKind::from_wire("fromGmail");
    assert_eq!(unknown.as_str(), "fromGmail");
}

#[test]
fn embedded_timezone_disagreeing_with_iana_records_its_source() {
    // calendar-semantics.md: a custom embedded VTIMEZONE is expanded from its own
    // rules; the engine records that the source is custom, not IANA.
    let custom = CalendarDateTime::Zoned {
        local: LocalDateTime::new(2021, 6, 1, 9, 0, 0).unwrap(),
        zone: TimeZoneId::custom("/Custom/Berlin").unwrap(),
    };
    assert_eq!(custom.zone().map(TimeZoneId::is_iana), Some(false));
}

#[test]
fn calendar_access_levels_and_default() {
    let owner = CalendarAccess::owner();
    assert!(owner.may_write && owner.may_delete);
    let reader = CalendarAccess::reader();
    assert!(reader.may_read && !reader.may_write);
    let fb = CalendarAccess::free_busy_only();
    assert!(!fb.may_read && fb.may_read_free_busy);
    assert_eq!(CalendarAccess::default(), CalendarAccess::owner());

    let mut cal = Calendar::new(CalendarId::try_from("cal-1").unwrap(), "Team");
    cal.access = reader;
    cal.default_alerts_with_time
        .push(Alert::display(Trigger::before_start(
            "PT5M".parse::<Duration>().unwrap(),
        )));
    cal.time_zone = Some(TimeZoneId::utc());
    cal.extended = ExtendedProperties::new();
    assert!(!cal.access.may_write);
}

#[test]
fn open_enums_display_and_preserve_unknowns() {
    // Cover the generated Display + Other path of the open-enum macro.
    assert_eq!(ParticipationStatus::Accepted.to_string(), "accepted");
    assert_eq!(EventStatus::Cancelled.to_string(), "cancelled");
    assert_eq!(AlertAction::Email.to_string(), "email");
    assert_eq!(RelativeTo::End, RelativeTo::End);
    let unknown = ParticipationStatus::from_wire("snoozed");
    assert_eq!(unknown.to_string(), "snoozed");
}
