//! Shared fixtures for the `CalendarEvent/set` tests: a stored event with its JSCalendar raw
//! preserved, the canned `/set` responses, and the neutral edit the host would state.
//!
//! The `/set` write path is covered by two sibling modules — `calendar_write_tests` (create,
//! destroy, the capability it advertises) and `calendar_patch_tests` (the PatchObject an
//! update produces) — because the second is where all the protocol detail lives.

use engine_core::{
    calendar::Event,
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    raw::RawJsCalendar,
    time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventEdit, EventPatch, PatchTarget};
use serde_json::{Value, json};

pub(super) const CALENDAR: &str = "b";
pub(super) const EVENT: &str = "l";

pub(super) fn calendar() -> CalendarId {
    CalendarId::try_from(CALENDAR).unwrap()
}

pub(super) fn uid() -> Uid {
    Uid::new("evt-1@test.local").unwrap()
}

pub(super) fn stamp() -> UtcDateTime {
    "2026-07-14T10:00:00Z".parse().unwrap()
}

pub(super) fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// An event as `sync_events` hands it back: zoned, with its JSCalendar raw preserved.
pub(super) fn stored(raw: &Value) -> Event {
    let mut event = Event::new(
        EventId::try_from(EVENT).unwrap(),
        uid(),
        Memberships::of_one(calendar()),
        zoned("2026-08-01T09:00:00"),
    );
    event.raw_jscalendar = Some(RawJsCalendar::new(raw.to_string()));
    event
}

/// The base event, with no location on it.
pub(super) fn base() -> Event {
    stored(&json!({
        "@type": "Event",
        "id": EVENT,
        "uid": "evt-1@test.local",
        "title": "Standup",
        "start": "2026-08-01T09:00:00",
        "timeZone": "Europe/Amsterdam",
        "duration": "PT30M",
    }))
}

pub(super) fn set_response(result: &Value) -> Value {
    json!({ "methodResponses": [["CalendarEvent/set", result, "0"]] })
}

pub(super) fn edit(base: &Event, target: PatchTarget, patch: EventPatch) -> EventEdit {
    EventEdit::new(base, target, patch)
}
