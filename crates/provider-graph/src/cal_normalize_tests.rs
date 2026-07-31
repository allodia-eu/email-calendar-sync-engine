//! Offline tests for the Graph calendar/event normalizers, driven by scrubbed real
//! Graph responses (`tests/fixtures/calendar/`).

use engine_core::{
    calendar::{EventStatus, Frequency, ParticipantRole, RecurrenceBound, Weekday},
    time::{CalendarDateTime, Duration, TimeZoneId},
};
use serde_json::Value;

use super::*;

const CALENDAR: &str = include_str!("../tests/fixtures/calendar/calendar.json");
const MASTER: &str = include_str!("../tests/fixtures/calendar/event_series_master.json");
const SINGLE: &str = include_str!("../tests/fixtures/calendar/event_single.json");
const ALLDAY: &str = include_str!("../tests/fixtures/calendar/event_allday.json");
const ONLINE: &str = include_str!("../tests/fixtures/calendar/event_online_meeting.json");
const CALENDARS: &str = include_str!("../tests/fixtures/calendar/calendars.json");
const EXTRA_EVENT: &str = include_str!("../tests/fixtures/calendar/event_extra_calendar.json");
const INVITATION: &str = include_str!("../tests/fixtures/calendar/event_invitation.json");

fn json(fixture: &str) -> Value {
    serde_json::from_str(fixture).unwrap()
}

fn calendar_id() -> CalendarId {
    CalendarId::try_from("cal-1").unwrap()
}

fn event(fixture: &str) -> Event {
    event_from_json(&json(fixture), &calendar_id()).unwrap()
}

#[test]
fn calendar_normalizes_name_default_and_owner() {
    let calendar = calendar_from_json(&json(CALENDAR)).unwrap();
    assert_eq!(calendar.name, "Calendar");
    assert!(calendar.is_default);
    assert_eq!(calendar.owner.as_deref(), Some("testuser@example.test"));
    assert!(calendar.id.key().as_str().starts_with("AAk"));
}

#[test]
fn series_master_maps_recurrence_windows_zone_and_preserves_raw() {
    let event = event(MASTER);
    assert_eq!(event.title, "PIM fixture: weekly standup");
    // The cross-system UID is the iCalUId, distinct from the provider EventId.
    assert!(event.uid.as_str().len() > 20 && event.id.as_str() != event.uid.as_str());

    // The sync requests times in the display zone (Prefer: outlook.timezone), so Graph
    // returns the IANA zone the engine expands in — the 09:00 authoring wall clock.
    let CalendarDateTime::Zoned { local, zone } = &event.start else {
        panic!("a timed event is zoned, got {:?}", event.start);
    };
    assert_eq!(zone, &TimeZoneId::iana("Europe/Amsterdam").unwrap());
    assert_eq!(local.hour(), 9);
    assert_eq!(event.duration, "PT30M".parse::<Duration>().unwrap());

    // patternedRecurrence → a weekly rule on Monday, first-day-of-week Sunday, bounded.
    assert!(event.is_recurring());
    let rule = &event.recurrence.as_ref().unwrap().rules[0];
    assert_eq!(rule.frequency, Frequency::Weekly);
    assert_eq!(rule.interval.get(), 1);
    assert_eq!(rule.by_day.len(), 1);
    assert_eq!(rule.by_day[0].day, Weekday::Mo);
    assert!(rule.by_day[0].nth_of_period.is_none());
    assert_eq!(rule.first_day_of_week, Weekday::Su);
    assert!(matches!(rule.bound, RecurrenceBound::Until(_)));

    assert_eq!(event.status, EventStatus::Confirmed);
    assert_eq!(event.locations.len(), 1);
    assert_eq!(event.locations[0].name.as_deref(), Some("Room A"));
    // The organizer rides as a participant with the owner role.
    assert!(
        event
            .participants
            .iter()
            .any(|p| p.has_role(&ParticipantRole::Owner))
    );
    // Revision tokens + the raw Graph payload are preserved beside the projection.
    assert!(event.revisions.etag.is_some());
    assert!(event.revisions.change_key.is_some());
    assert!(event.extended.get("microsoft.graph/event").is_some());
    assert!(event.raw_ical.is_none() && event.raw_jscalendar.is_none());
}

#[test]
fn single_instance_is_not_recurring_and_carries_attendees() {
    let event = event(SINGLE);
    assert_eq!(event.title, "PIM fixture: one-off meeting");
    assert!(!event.is_recurring());
    assert!(matches!(event.start, CalendarDateTime::Zoned { .. }));
    // The invited attendee (a non-account address, unscrubbed) is projected.
    assert!(
        event
            .participants
            .iter()
            .any(|p| p.email.as_deref() == Some("bob@example.test")
                && p.has_role(&ParticipantRole::Attendee))
    );
}

#[test]
fn all_day_event_is_a_zoneless_date_with_a_day_duration() {
    let event = event(ALLDAY);
    // An all-day event is a zoneless calendar date, never a zoned/UTC instant.
    let CalendarDateTime::Date(date) = &event.start else {
        panic!("an all-day event is a Date, got {:?}", event.start);
    };
    assert_eq!(date.to_string(), "2026-08-10");
    assert!(event.is_all_day());
    // The exclusive end (the 11th) yields a one-day duration.
    assert_eq!(event.duration, "P1D".parse::<Duration>().unwrap());
}

#[test]
fn an_online_meeting_projects_its_join_url_and_preserves_the_teams_payload() {
    // This fixture is a real Teams "for business" meeting. The join URL is already
    // projected as a virtual location; the online-meeting *provider identity*
    // (`onlineMeetingProvider`/`isOnlineMeeting`) is not modelled yet (staged), so the
    // preserved raw payload is what a future provider-typing mapper reads.
    let event = event(ONLINE);
    assert_eq!(event.title, "PIM fixture: Teams online meeting");
    assert!(!event.is_recurring());
    assert!(matches!(event.start, CalendarDateTime::Zoned { .. }));

    // The join URL is surfaced as a virtual location today.
    assert_eq!(event.virtual_locations.len(), 1);
    assert!(
        event.virtual_locations[0]
            .uri
            .starts_with("https://teams.live.com/meet/")
    );

    // The provider-typed fields ride the preserved raw payload until they are modelled.
    let raw = event
        .extended
        .get("microsoft.graph/event")
        .expect("raw Graph event preserved");
    assert_eq!(raw["isOnlineMeeting"], Value::Bool(true));
    assert_eq!(raw["onlineMeetingProvider"], "teamsForBusiness");
}

#[test]
fn a_non_default_calendar_normalizes_with_its_own_id_and_color() {
    // One MS account exposes several calendars; each is a distinct container with its own
    // id, name, default flag, and `#rrggbb` color (Graph's `hexColor` wins over `color`).
    let list = json(CALENDARS);
    let entries = list["value"].as_array().unwrap();
    let default = calendar_from_json(&entries[0]).unwrap();
    let extra = calendar_from_json(&entries[1]).unwrap();
    assert!(default.is_default && default.color.is_none());
    assert_eq!(extra.name, "Extra calendar test");
    assert!(!extra.is_default);
    assert_eq!(extra.color.as_deref(), Some("#f7630c"));
    assert_ne!(extra.id, default.id);
}

#[test]
fn an_event_is_bound_to_the_calendar_it_was_fetched_under() {
    // Graph event JSON does not name its own calendar, so the normalizer binds the event
    // to the calendar it was fetched from. That membership is how events from multiple
    // calendars under one account stay separable and never mix.
    let extra = CalendarId::try_from("extra-cal").unwrap();
    let event = event_from_json(&json(EXTRA_EVENT), &extra).unwrap();
    assert_eq!(event.title, "PIM fixture: event on a secondary calendar");
    assert_eq!(event.calendars.len().get(), 1);
    assert!(event.calendars.contains(&extra));
    assert!(!event.calendars.contains(&calendar_id()));
}

#[test]
fn a_malformed_event_is_a_protocol_error_not_a_panic() {
    // No id.
    assert!(event_from_json(&serde_json::json!({ "iCalUId": "u" }), &calendar_id()).is_err());
    // No iCalUId/uid.
    assert!(event_from_json(&serde_json::json!({ "id": "e" }), &calendar_id()).is_err());
    // A start with a bad dateTime.
    assert!(
        event_from_json(
            &serde_json::json!({
                "id": "e", "iCalUId": "u",
                "start": { "dateTime": "not-a-date", "timeZone": "UTC" },
                "end": { "dateTime": "2026-08-01T10:00:00", "timeZone": "UTC" }
            }),
            &calendar_id()
        )
        .is_err()
    );
}

#[test]
fn a_windows_zone_name_maps_through_the_cldr_table() {
    // A sync without `Prefer: outlook.timezone` gets Graph's default Windows names; those
    // still resolve to IANA through the CLDR table.
    let event = event_from_json(
        &serde_json::json!({
            "id": "e", "iCalUId": "u", "subject": "windows zone",
            "start": { "dateTime": "2026-08-01T09:00:00", "timeZone": "W. Europe Standard Time" },
            "end": { "dateTime": "2026-08-01T10:00:00", "timeZone": "W. Europe Standard Time" }
        }),
        &calendar_id(),
    )
    .unwrap();
    assert_eq!(
        event.start.zone(),
        Some(&TimeZoneId::iana("Europe/Berlin").unwrap())
    );
}

#[test]
fn an_invitation_holds_one_participant_per_address_with_my_answer() {
    // The **invitee's** copy of a meeting, captured live: unlike the organizer's own copy
    // (`event_single.json`, where `attendees` excludes the organizer), Graph lists the
    // organizer *both* as `organizer` and as an `attendees[]` entry. One participant per
    // address, roles unioned — see `participants`.
    let event = event(INVITATION);
    assert_eq!(event.participants.len(), 2, "{:?}", event.participants);

    let organizer = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("boss@example.test"))
        .unwrap();
    assert!(organizer.has_role(&ParticipantRole::Owner));
    assert!(organizer.has_role(&ParticipantRole::Attendee));
    // Graph writes `"none"` for the organizer's own entry — it never tracks an organizer's
    // response — so the owner's implied acceptance stands rather than being downgraded to
    // "hasn't answered". (Google differs: it *does* track it, so there the entry wins.)
    assert_eq!(
        organizer.participation_status,
        ParticipationStatus::Accepted
    );

    // My own answer is reported as answered, from my `attendees[]` entry.
    let me = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("testuser@example.test"))
        .unwrap();
    assert_eq!(me.participation_status, ParticipationStatus::Accepted);
    assert!(!me.has_role(&ParticipantRole::Owner), "not my meeting");
}

#[test]
fn a_real_attendee_who_has_not_answered_still_reads_as_needs_action() {
    // The organizer-entry carve-out above must not swallow a *guest's* `"none"`, which
    // genuinely means "has not responded" — the organizer's own copy in `event_single.json`
    // has exactly that guest.
    let event = event(SINGLE);
    let bob = event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("bob@example.test"))
        .unwrap();
    assert_eq!(bob.participation_status, ParticipationStatus::NeedsAction);
    // And in the organizer's own copy Graph omits the organizer from `attendees`, so there
    // is nothing to merge: one owner participant plus the guest.
    assert_eq!(event.participants.len(), 2);
}

#[test]
fn an_unknown_windows_zone_is_preserved_as_custom() {
    // A zone name absent from CLDR is preserved as a custom zone (not guessed), so the
    // event stores even though the expander will not yet expand a custom zone.
    let event = event_from_json(
        &serde_json::json!({
            "id": "e", "iCalUId": "u", "subject": "custom zone",
            "start": { "dateTime": "2026-08-01T09:00:00", "timeZone": "tzone://Microsoft/Custom" },
            "end": { "dateTime": "2026-08-01T10:00:00", "timeZone": "tzone://Microsoft/Custom" }
        }),
        &calendar_id(),
    )
    .unwrap();
    let CalendarDateTime::Zoned { zone, .. } = &event.start else {
        panic!("zoned");
    };
    assert!(!zone.is_iana());
    assert_eq!(zone.as_str(), "tzone://Microsoft/Custom");
}
