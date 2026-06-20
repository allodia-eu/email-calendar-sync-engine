//! Mapping a single `VEVENT` component into the engine's [`Event`] projection,
//! and folding a `RECURRENCE-ID` override `VEVENT` into a master's overrides.
//!
//! The time model follows `value.rs` (`DTSTART` + `TZID`/`Z`/neither →
//! zoned/UTC/floating, or a `DATE` all-day value); the length is `DTEND −
//! DTSTART` or an explicit `DURATION` (RFC 5545 §3.6.1). Enum spellings that
//! differ from JSCalendar (`STATUS`, `TRANSP`, `CLASS`) are mapped explicitly so
//! the projection matches the JMAP adapter's. Identity and calendar membership
//! are supplied by the caller (from the resource href and collection), since a
//! `VEVENT` body does not carry them.

use std::collections::BTreeSet;

use engine_core::calendar::{Event, EventStatus, FreeBusyStatus, Privacy};
use engine_core::ids::{CalendarId, EventId, Uid};
use engine_core::membership::Memberships;
use engine_core::raw::RawIcal;
use engine_core::time::{CalendarDateTime, Duration, UtcDateTime};

use super::component::Component;
use super::party::{parse_conferences, parse_locations, parse_participants};
use super::recurrence::parse_recurrence;
use super::unfold::unescape_text;
use super::value::{parse_calendar_date_time, parse_duration, parse_utc};
use crate::error::CalDavError;

/// The cross-system `UID` of a `VEVENT`.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the `UID` is missing or empty.
pub(crate) fn vevent_uid(vevent: &Component) -> Result<Uid, CalDavError> {
    let uid = vevent
        .value("UID")
        .map(str::trim)
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| CalDavError::ical("VEVENT missing UID"))?;
    Uid::new(uid).map_err(|e| CalDavError::ical(format!("bad UID: {e}")))
}

/// The `RECURRENCE-ID` of a `VEVENT`, present only on an override instance.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the value is unparseable.
pub(super) fn recurrence_id_of(
    vevent: &Component,
) -> Result<Option<CalendarDateTime>, CalDavError> {
    match vevent.property("RECURRENCE-ID") {
        Some(line) => Ok(Some(parse_calendar_date_time(line)?)),
        None => Ok(None),
    }
}

/// Maps a master `VEVENT` into an [`Event`], with identity, calendar membership,
/// and the preserved raw resource supplied by the caller.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] on a missing/invalid `DTSTART`/`UID` or any
/// unparseable time, duration, or recurrence value.
pub(crate) fn event_from_vevent(
    vevent: &Component,
    id: EventId,
    calendar: CalendarId,
    raw_ical: RawIcal,
) -> Result<Event, CalDavError> {
    let uid = vevent_uid(vevent)?;
    let start = parse_start(vevent)?;
    let duration = event_duration(vevent, &start)?;
    let mut event = Event::new(id, uid, Memberships::of_one(calendar), start);
    event.duration = duration;
    event.title = vevent
        .value("SUMMARY")
        .map(unescape_text)
        .unwrap_or_default();
    event.description = vevent.value("DESCRIPTION").map(unescape_text);
    // A present-but-empty STATUS/TRANSP/CLASS is treated as absent (keeping the
    // model default), not mapped to an empty open-enum variant or a wrong default.
    if let Some(status) = nonempty(vevent.value("STATUS")) {
        event.status = map_status(status);
    }
    if let Some(transp) = nonempty(vevent.value("TRANSP")) {
        event.free_busy_status = map_transp(transp);
    }
    if let Some(class) = nonempty(vevent.value("CLASS")) {
        event.privacy = map_class(class);
    }
    event.sequence = parse_u32(vevent.value("SEQUENCE"));
    event.priority = parse_u8(vevent.value("PRIORITY"));
    event.recurrence_id = recurrence_id_of(vevent)?;
    // Recurrence rules live only on the series master, never on an override.
    // Parsing is best-effort: a malformed RRULE or EXDATE degrades to no/partial
    // recurrence rather than dropping the whole event (`calendar-semantics.md`).
    if event.recurrence_id.is_none() {
        event.recurrence = parse_recurrence(vevent, &event.start);
    }
    event.participants = parse_participants(vevent);
    event.locations = parse_locations(vevent);
    event.virtual_locations = parse_conferences(vevent);
    event.categories = category_set(vevent);
    event.color = nonempty(vevent.value("COLOR")).map(str::to_owned);
    event.created = opt_utc(vevent, "CREATED")?;
    event.updated = opt_utc(vevent, "LAST-MODIFIED")?;
    event.raw_ical = Some(raw_ical);
    Ok(event)
}

/// Parses the mandatory `DTSTART`.
fn parse_start(vevent: &Component) -> Result<CalendarDateTime, CalDavError> {
    let line = vevent
        .property("DTSTART")
        .ok_or_else(|| CalDavError::ical("VEVENT missing DTSTART"))?;
    parse_calendar_date_time(line)
}

/// Derives the event length from `DTEND`, else an explicit `DURATION`, else the
/// RFC 5545 §3.6.1 default (one day for an all-day start, otherwise zero).
fn event_duration(vevent: &Component, start: &CalendarDateTime) -> Result<Duration, CalDavError> {
    if let Some(dtend) = vevent.property("DTEND") {
        let end = parse_calendar_date_time(dtend)?;
        return start
            .duration_until(&end)
            .map_err(|e| CalDavError::ical(format!("bad DTSTART/DTEND span: {e}")));
    }
    if let Some(duration) = vevent.value("DURATION") {
        return parse_duration(duration);
    }
    if start.is_all_day() {
        return Duration::from_parts(0, 1, 0, 0, 0, 0)
            .map_err(|e| CalDavError::ical(format!("one-day default: {e}")));
    }
    Ok(Duration::ZERO)
}

/// The set of `CATEGORIES` (each property may carry a comma-separated list).
fn category_set(vevent: &Component) -> BTreeSet<String> {
    vevent
        .all_properties("CATEGORIES")
        .flat_map(|line| line.value.split(','))
        .map(|category| unescape_text(category.trim()))
        .filter(|category| !category.is_empty())
        .collect()
}

/// Parses a `CREATED`/`LAST-MODIFIED` UTC timestamp, if present.
fn opt_utc(vevent: &Component, name: &str) -> Result<Option<UtcDateTime>, CalDavError> {
    match vevent.value(name) {
        Some(value) => parse_utc(value).map(Some),
        None => Ok(None),
    }
}

/// Maps `STATUS` to [`EventStatus`].
fn map_status(value: &str) -> EventStatus {
    match value.trim().to_ascii_uppercase().as_str() {
        "CONFIRMED" => EventStatus::Confirmed,
        "CANCELLED" => EventStatus::Cancelled,
        "TENTATIVE" => EventStatus::Tentative,
        other => EventStatus::from_wire(&other.to_ascii_lowercase()),
    }
}

/// Maps `TRANSP` to [`FreeBusyStatus`] (`TRANSPARENT` frees time; `OPAQUE` blocks).
fn map_transp(value: &str) -> FreeBusyStatus {
    if value.trim().eq_ignore_ascii_case("TRANSPARENT") {
        FreeBusyStatus::Free
    } else {
        FreeBusyStatus::Busy
    }
}

/// Maps `CLASS` to [`Privacy`] (`CONFIDENTIAL` → `secret`).
fn map_class(value: &str) -> Privacy {
    match value.trim().to_ascii_uppercase().as_str() {
        "PUBLIC" => Privacy::Public,
        "PRIVATE" => Privacy::Private,
        "CONFIDENTIAL" => Privacy::Secret,
        other => Privacy::from_wire(&other.to_ascii_lowercase()),
    }
}

/// Parses a `u32` property (`SEQUENCE`), defaulting to 0.
fn parse_u32(value: Option<&str>) -> u32 {
    value.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Parses a `u8` property (`PRIORITY`), defaulting to 0.
fn parse_u8(value: Option<&str>) -> u8 {
    value.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Treats a present-but-empty (or whitespace-only) property value as absent.
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::component::parse_components;
    use super::*;

    fn vevents(text: &str) -> Vec<Component> {
        parse_components(text)
            .into_iter()
            .flat_map(|c| c.children)
            .filter(|c| c.name == "VEVENT")
            .collect()
    }

    fn ids() -> (EventId, CalendarId) {
        (
            EventId::try_from("/cal/x.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
    }

    #[test]
    fn maps_status_transp_class_spellings() {
        assert_eq!(map_status("CANCELLED"), EventStatus::Cancelled);
        assert_eq!(map_transp("TRANSPARENT"), FreeBusyStatus::Free);
        assert_eq!(map_transp("OPAQUE"), FreeBusyStatus::Busy);
        assert_eq!(map_class("CONFIDENTIAL"), Privacy::Secret);
    }

    #[test]
    fn all_day_event_with_no_end_defaults_to_one_day() {
        let vevent = &vevents(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a@x\r\nDTSTART;VALUE=DATE:20260401\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        )[0];
        let (id, cal) = ids();
        let event = event_from_vevent(vevent, id, cal, RawIcal::new("x")).unwrap();
        assert!(event.is_all_day());
        assert_eq!(event.duration, "P1D".parse().unwrap());
    }

    #[test]
    fn maps_full_metadata_status_class_duration_and_timestamps() {
        // A VEVENT exercising the metadata the seed omits: STATUS/CLASS, an
        // explicit DURATION (no DTEND), SEQUENCE/PRIORITY, CATEGORIES, COLOR, and
        // CREATED/LAST-MODIFIED timestamps.
        let vevent = &vevents(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:meta@x\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nDURATION:PT45M\r\n\
             SUMMARY:Quarterly review\r\nDESCRIPTION:Line one\\nLine two\r\n\
             STATUS:TENTATIVE\r\nCLASS:CONFIDENTIAL\r\nTRANSP:OPAQUE\r\n\
             SEQUENCE:4\r\nPRIORITY:1\r\nCATEGORIES:Work,Finance\r\nCOLOR:thistle\r\n\
             CREATED:20260101T080000Z\r\nLAST-MODIFIED:20260102T093000Z\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
        )[0];
        let (id, cal) = ids();
        let event = event_from_vevent(vevent, id, cal, RawIcal::new("x")).unwrap();

        assert_eq!(event.duration, "PT45M".parse().unwrap());
        assert_eq!(event.description.as_deref(), Some("Line one\nLine two"));
        assert_eq!(event.status, EventStatus::Tentative);
        assert_eq!(event.privacy, Privacy::Secret); // CONFIDENTIAL → secret
        assert_eq!(event.free_busy_status, FreeBusyStatus::Busy); // OPAQUE → busy
        assert_eq!(event.sequence, 4);
        assert_eq!(event.priority, 1);
        assert_eq!(event.color.as_deref(), Some("thistle"));
        assert_eq!(
            event.categories,
            ["Finance".to_owned(), "Work".to_owned()]
                .into_iter()
                .collect()
        );
        assert_eq!(event.created.unwrap().to_string(), "2026-01-01T08:00:00Z");
        assert_eq!(event.updated.unwrap().to_string(), "2026-01-02T09:30:00Z");
    }

    #[test]
    fn unknown_status_and_class_fall_back_to_other() {
        // An out-of-spec STATUS/CLASS is preserved verbatim via the open enums.
        assert!(matches!(map_status("NEW-PHASE"), EventStatus::Other(v) if v == "new-phase"));
        assert!(matches!(map_class("INTERNAL"), Privacy::Other(v) if v == "internal"));
    }

    #[test]
    fn empty_status_transp_class_color_are_treated_as_absent() {
        let vevent = &vevents(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:e@x\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n\
             STATUS:\r\nTRANSP:\r\nCLASS:\r\nCOLOR:\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        )[0];
        let (id, cal) = ids();
        let event = event_from_vevent(vevent, id, cal, RawIcal::new("x")).unwrap();
        // Defaults preserved; no Other("") pollution.
        assert_eq!(event.status, EventStatus::Confirmed);
        assert_eq!(event.free_busy_status, FreeBusyStatus::Busy);
        assert_eq!(event.privacy, Privacy::Public);
        assert_eq!(event.color, None);
    }
}
