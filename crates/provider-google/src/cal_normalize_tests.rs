//! Offline calendar-normalization tests, driven by the captured (scrubbed) calendar
//! fixtures under `tests/fixtures/calendar/`.

use engine_core::calendar::{
    EventStatus, FreeBusyStatus, Frequency, ParticipantRole, ParticipationStatus, RecurrenceBound,
};
use serde_json::Value;

use super::*;

const CALENDARS: &str = include_str!("../tests/fixtures/calendar/calendars.json");
const SINGLE: &str = include_str!("../tests/fixtures/calendar/event_single.json");
const RECURRING: &str = include_str!("../tests/fixtures/calendar/event_recurring_master.json");
const ALLDAY: &str = include_str!("../tests/fixtures/calendar/event_allday.json");
const MEET: &str = include_str!("../tests/fixtures/calendar/event_meet.json");
const ORGANIZER_DECLINED: &str =
    include_str!("../tests/fixtures/calendar/event_organizer_declined.json");
const INVITATION_ANSWERED: &str =
    include_str!("../tests/fixtures/calendar/event_invitation_answered.json");

fn calendar() -> CalendarId {
    CalendarId::try_from("primary").unwrap()
}

fn event(fixture: &str) -> Event {
    event_from_json(&serde_json::from_str(fixture).unwrap(), &calendar(), None).unwrap()
}

#[test]
fn calendar_list_maps_primary_and_reader_roles() {
    let doc: Value = serde_json::from_str(CALENDARS).unwrap();
    let calendars: Vec<Calendar> = doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| calendar_from_json(c).unwrap())
        .collect();
    assert_eq!(calendars.len(), 2);
    let primary = calendars.iter().find(|c| c.is_default).unwrap();
    assert_eq!(primary.color.as_deref(), Some("#9fe1e7"));
    assert!(primary.access.may_write); // owner
    assert!(primary.owner.is_some()); // the primary id is the account address
    assert_eq!(
        primary.time_zone.as_ref().unwrap().as_str(),
        "Europe/Amsterdam"
    );
    // The holiday calendar is a read-only subscription.
    let holiday = calendars.iter().find(|c| !c.is_default).unwrap();
    assert!(!holiday.access.may_write);
    assert!(holiday.owner.is_none());
}

#[test]
fn single_event_normalizes_zoned_time_participants_and_location() {
    let event = event(SINGLE);
    assert_eq!(event.title, "Fixture: single meeting");
    assert_eq!(event.status, EventStatus::Confirmed);
    assert_eq!(event.free_busy_status, FreeBusyStatus::Busy);
    // IANA-native: the wall clock is 10:00 in Europe/Amsterdam (no Windows-zone table).
    assert_eq!(
        event
            .start
            .zone()
            .map(engine_core::time::TimeZoneId::as_str),
        Some("Europe/Amsterdam")
    );
    assert_eq!(event.start.local().unwrap().hour(), 10);
    // A one-hour meeting.
    assert!(!event.duration.is_zero());
    assert_eq!(event.locations[0].name.as_deref(), Some("Room 1"));
    // The organizer is the owner (accepted); the guest still needs to respond.
    let organizer = event
        .participants
        .iter()
        .find(|p| p.roles.contains(&ParticipantRole::Owner))
        .unwrap();
    assert_eq!(
        organizer.participation_status,
        ParticipationStatus::Accepted
    );
    let guest = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("guest@example.test"))
        .unwrap();
    assert_eq!(guest.participation_status, ParticipationStatus::NeedsAction);
    // The raw Google event rides beside the projection.
    assert!(event.extended.get("google/event").is_some());
}

#[test]
fn recurring_master_parses_the_rrule() {
    let event = event(RECURRING);
    let recurrence = event.recurrence.expect("a recurring master");
    assert_eq!(recurrence.rules.len(), 1);
    let rule = &recurrence.rules[0];
    assert_eq!(rule.frequency, Frequency::Weekly);
    assert!(matches!(rule.bound, RecurrenceBound::Count(n) if n.get() == 6));
    assert_eq!(rule.by_day.len(), 1);
    // The uid comes from iCalUID.
    assert!(event.uid.as_str().ends_with("@google.com"));
}

#[test]
fn all_day_event_is_a_zoneless_date() {
    let event = event(ALLDAY);
    assert!(event.start.is_all_day());
    assert!(event.start.zone().is_none());
    // A one-day span (start 2026-08-05, end 2026-08-06).
    assert!(!event.duration.is_zero());
}

#[test]
fn meet_event_projects_the_join_link_as_a_virtual_location() {
    let event = event(MEET);
    assert_eq!(event.virtual_locations.len(), 1);
    assert!(event.virtual_locations[0].uri.contains("meet.google.com"));
}

#[test]
fn a_single_event_falls_back_to_the_event_id_when_ical_uid_is_absent() {
    let json = serde_json::json!({
        "id": "evt-1", "summary": "No UID",
        "start": { "dateTime": "2026-08-03T10:00:00+02:00", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-08-03T11:00:00+02:00", "timeZone": "Europe/Amsterdam" }
    });
    let event = event_from_json(&json, &calendar(), None).unwrap();
    assert_eq!(event.uid.as_str(), "evt-1");
}

#[test]
fn an_endpoint_without_a_zone_falls_back_to_the_default_then_utc() {
    let json = serde_json::json!({
        "id": "evt-2", "summary": "Zoneless",
        "start": { "dateTime": "2026-08-03T10:00:00Z" },
        "end": { "dateTime": "2026-08-03T11:00:00Z" }
    });
    // With a default zone supplied, the endpoint adopts it.
    let with_default = event_from_json(&json, &calendar(), Some("Europe/Amsterdam")).unwrap();
    assert_eq!(
        with_default
            .start
            .zone()
            .map(engine_core::time::TimeZoneId::as_str),
        Some("Europe/Amsterdam")
    );
    // With none, it falls back to UTC.
    let utc = event_from_json(&json, &calendar(), None).unwrap();
    assert_eq!(
        utc.start.zone().map(engine_core::time::TimeZoneId::as_str),
        Some("UTC")
    );
}

#[test]
fn optional_and_resource_attendees_transparency_and_visibility_map() {
    let json = serde_json::json!({
        "id": "e", "summary": "Full", "status": "tentative",
        "transparency": "transparent", "visibility": "private", "description": "notes",
        "start": { "dateTime": "2026-08-03T10:00:00+02:00", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-08-03T11:00:00+02:00", "timeZone": "Europe/Amsterdam" },
        "organizer": { "email": "org@example.test", "displayName": "Org" },
        "attendees": [
            { "email": "opt@example.test", "optional": true, "responseStatus": "tentative" },
            { "email": "room@example.test", "resource": true, "responseStatus": "accepted" },
            { "email": "dec@example.test", "responseStatus": "declined" }
        ]
    });
    let event = event_from_json(&json, &calendar(), None).unwrap();
    assert_eq!(event.status, EventStatus::Tentative);
    assert_eq!(
        event.free_busy_status,
        engine_core::calendar::FreeBusyStatus::Free
    );
    assert_eq!(event.privacy, engine_core::calendar::Privacy::Private);
    assert_eq!(event.description.as_deref(), Some("notes"));
    let opt = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("opt@example.test"))
        .unwrap();
    assert!(opt.roles.contains(&ParticipantRole::Optional));
    assert_eq!(opt.participation_status, ParticipationStatus::Tentative);
    let room = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("room@example.test"))
        .unwrap();
    assert_eq!(
        room.kind,
        Some(engine_core::calendar::ParticipantKind::Resource)
    );
    let dec = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("dec@example.test"))
        .unwrap();
    assert_eq!(dec.participation_status, ParticipationStatus::Declined);
}

#[test]
fn a_confidential_event_is_secret_and_meet_falls_back_to_an_entry_point() {
    // No hangoutLink, but a conferenceData video entry point → still a virtual location.
    let json = serde_json::json!({
        "id": "e", "summary": "Conf", "visibility": "confidential",
        "start": { "dateTime": "2026-08-03T10:00:00+02:00", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-08-03T11:00:00+02:00", "timeZone": "Europe/Amsterdam" },
        "conferenceData": { "entryPoints": [
            { "entryPointType": "phone", "uri": "tel:+100" },
            { "entryPointType": "video", "uri": "https://meet.google.com/xyz" }
        ]}
    });
    let event = event_from_json(&json, &calendar(), None).unwrap();
    assert_eq!(event.privacy, engine_core::calendar::Privacy::Secret);
    assert_eq!(event.virtual_locations.len(), 1);
    assert!(
        event.virtual_locations[0]
            .uri
            .contains("meet.google.com/xyz")
    );
}

#[test]
fn the_organizer_who_is_also_a_guest_is_one_participant_carrying_the_answer() {
    // Google names the organizer twice — the `organizer` object *and*, when they are also
    // invited, an `attendees[]` entry (`"organizer": true, "self": true`). The projection is
    // one participant per address with a set of roles, so the two are one participant whose
    // status is the one the server holds. Answering an invitation writes exactly that
    // attendee entry, so a duplicate would let a host read the stale `accepted` of a
    // synthesized organizer instead of the answer it just sent (live-proven; see
    // `tests/fixtures/README.md`).
    let event = event(ORGANIZER_DECLINED);
    let mine: Vec<_> = event
        .participants
        .iter()
        .filter(|p| p.email.as_deref() == Some("testuser@example.test"))
        .collect();
    assert_eq!(mine.len(), 1, "one participant per address: {mine:?}");
    assert!(mine[0].roles.contains(&ParticipantRole::Owner));
    assert!(mine[0].roles.contains(&ParticipantRole::Attendee));
    assert_eq!(
        mine[0].participation_status,
        ParticipationStatus::Declined,
        "the answer the server holds wins over the organizer's implied acceptance"
    );
    // A self-organized event with no other guest has exactly that one participant.
    assert_eq!(event.participants.len(), 1);
}

#[test]
fn an_answered_invitation_reports_my_status_and_merges_the_foreign_organizer() {
    // An event *organized by someone else* (imported with a foreign organizer) that the
    // account answered `tentative`. The organizer is likewise named twice, so the same merge
    // applies to them; my own entry carries the answer.
    let event = event(INVITATION_ANSWERED);
    assert_eq!(event.participants.len(), 2, "{:?}", event.participants);
    let me = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("testuser@example.test"))
        .unwrap();
    assert_eq!(me.participation_status, ParticipationStatus::Tentative);
    assert!(
        !me.roles.contains(&ParticipantRole::Owner),
        "not my meeting"
    );
    let boss = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("boss@example.test"))
        .unwrap();
    assert!(boss.roles.contains(&ParticipantRole::Owner));
    assert!(boss.roles.contains(&ParticipantRole::Attendee));
    assert_eq!(boss.participation_status, ParticipationStatus::Accepted);
    assert_eq!(boss.name.as_deref(), Some("The Boss"));
}

#[test]
fn malformed_events_are_protocol_errors_not_panics() {
    assert!(event_from_json(&serde_json::json!({}), &calendar(), None).is_err());
    // A bad RRULE surfaces as a protocol error.
    assert!(
        event_from_json(
            &serde_json::json!({
                "id": "e", "summary": "x", "recurrence": ["RRULE:FREQ=NOPE"],
                "start": { "date": "2026-08-05" }, "end": { "date": "2026-08-06" }
            }),
            &calendar(),
            None
        )
        .is_err()
    );
}
