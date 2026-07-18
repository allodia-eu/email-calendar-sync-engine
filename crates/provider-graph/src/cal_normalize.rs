//! Normalizing Microsoft Graph `calendar` and `event` JSON into the engine calendar
//! model.
//!
//! Graph events are neither iCalendar nor JSCalendar, so the projection maps every axis
//! faithfully and preserves the **raw Graph event** beside it in
//! [`Event::extended`](engine_core::calendar::Event::extended) (the model invariant that
//! raw is kept beside the lossy projection — `calendar-semantics.md`). Two Graph
//! realities shape it: event `start`/`end` are `{ dateTime, timeZone }` with a
//! **Windows** zone name (mapped to IANA at this boundary — [`crate::windows_zones`]),
//! and recurrence is a structured `patternedRecurrence` (mapped in [`crate::cal_recur`]),
//! not an `RRULE` string.
//!
//! Only `seriesMaster` and `singleInstance` events are projected; a server-expanded
//! `occurrence` (the engine expands the master itself) and an `exception` (Graph v1.0
//! exposes no recurrence-id to key an override on) are filtered out upstream in
//! [`crate::cal_fetch`] — see the `graph.md` calendar limitations.

use engine_core::{
    calendar::{
        Calendar, Event, EventStatus, FreeBusyStatus, Location, Participant, ParticipantKind,
        ParticipantRole, ParticipationStatus, Privacy, VirtualLocation,
    },
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId},
    version::{ChangeKey, ETag, RevisionTokens},
};
use serde_json::Value;

use crate::{
    cal_recur::parse_recurrence,
    error::GraphError,
    json::{bool_field, datetime, opt_str, req_str, wrap_id},
    windows_zones::windows_to_iana,
};

/// The namespaced key under which the whole raw Graph event JSON is preserved.
const RAW_EVENT_KEY: &str = "microsoft.graph/event";

/// Normalizes one Graph `calendar` into a [`Calendar`] container.
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] if the object lacks a usable `id`.
pub(crate) fn calendar_from_json(value: &Value) -> Result<Calendar, GraphError> {
    let id = wrap_id(CalendarId::try_from(req_str(value, "id")?), "calendar id")?;
    let mut calendar = Calendar::new(id, opt_str(value, "name").unwrap_or_default());
    // `hexColor` is a concrete `#rrggbb`; `color` is a named category ("auto", "lightBlue").
    calendar.color = opt_str(value, "hexColor")
        .filter(|c| !c.is_empty())
        .or_else(|| opt_str(value, "color").filter(|c| *c != "auto"))
        .map(str::to_owned);
    calendar.is_default = bool_field(value, "isDefaultCalendar");
    calendar.owner = value
        .get("owner")
        .and_then(|o| opt_str(o, "address"))
        .map(str::to_owned);
    calendar.revisions = revisions(value);
    Ok(calendar)
}

/// Normalizes one Graph `event` (`seriesMaster` or `singleInstance`) into an [`Event`]
/// belonging to `calendar` (the bound collection — Graph event JSON does not name its
/// own calendar).
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] on a missing `id`/`iCalUId`, an unparseable
/// time/duration/recurrence, or a malformed field.
pub(crate) fn event_from_json(value: &Value, calendar: &CalendarId) -> Result<Event, GraphError> {
    let id = wrap_id(EventId::try_from(req_str(value, "id")?), "event id")?;
    let uid_raw = opt_str(value, "iCalUId")
        .or_else(|| opt_str(value, "uid"))
        .ok_or_else(|| GraphError::protocol("event has no iCalUId"))?;
    let uid = Uid::new(uid_raw).map_err(|e| GraphError::protocol(format!("bad event uid: {e}")))?;

    let all_day = bool_field(value, "isAllDay");
    let start = parse_endpoint(value, "start", all_day)?;
    let end = parse_endpoint(value, "end", all_day)?;
    let duration = start
        .duration_until(&end)
        .map_err(|e| GraphError::protocol(format!("bad event start/end: {e}")))?;

    let mut event = Event::new(id, uid, Memberships::of_one(calendar.clone()), start);
    event.duration = duration;
    opt_str(value, "subject")
        .unwrap_or_default()
        .clone_into(&mut event.title);
    event.description = description(value);
    event.status = if bool_field(value, "isCancelled") {
        EventStatus::Cancelled
    } else {
        EventStatus::Confirmed
    };
    event.free_busy_status = match opt_str(value, "showAs") {
        Some("free") => FreeBusyStatus::Free,
        _ => FreeBusyStatus::Busy,
    };
    event.privacy = match opt_str(value, "sensitivity") {
        Some("private") => Privacy::Private,
        Some("confidential") => Privacy::Secret,
        _ => Privacy::Public,
    };
    event.created = datetime(value, "createdDateTime")?;
    event.updated = datetime(value, "lastModifiedDateTime")?;
    event.recurrence = parse_recurrence(value)?;
    event.participants = participants(value);
    event.locations = locations(value);
    event.virtual_locations = virtual_locations(value);
    event.categories = value
        .get("categories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    event.revisions = revisions(value);
    // Preserve the provider-native payload beside the projection (Graph is neither
    // iCal nor JSCalendar, so it rides `extended`, not `raw_ical`/`raw_jscalendar`).
    event.extended.set(RAW_EVENT_KEY, value.clone());
    Ok(event)
}

/// Parses a Graph `{ dateTime, timeZone }` endpoint into a [`CalendarDateTime`]: an
/// all-day event is a zoneless [`Date`](CalendarDateTime::Date); a timed event is
/// [`Zoned`](CalendarDateTime::Zoned) with the Windows zone mapped to IANA.
fn parse_endpoint(value: &Value, key: &str, all_day: bool) -> Result<CalendarDateTime, GraphError> {
    let obj = value
        .get(key)
        .ok_or_else(|| GraphError::protocol(format!("event has no {key}")))?;
    let raw = req_str(obj, "dateTime")?;
    let local: LocalDateTime = raw
        .parse()
        .map_err(|e| GraphError::protocol(format!("bad {key} dateTime {raw:?}: {e}")))?;
    if all_day {
        let date = CalendarDate::new(local.year(), local.month(), local.day())
            .map_err(|e| GraphError::protocol(format!("bad all-day {key}: {e}")))?;
        return Ok(CalendarDateTime::Date(date));
    }
    let zone = resolve_zone(opt_str(obj, "timeZone").unwrap_or("UTC"))?;
    Ok(CalendarDateTime::Zoned { local, zone })
}

/// Resolves a Graph zone name to a [`TimeZoneId`]. Graph returns a **Windows** name by
/// default and the requested **IANA** name when the sync sent `Prefer: outlook.timezone`
/// (an IANA zone — [`crate::cal_fetch`]), so both are accepted: a known Windows name maps
/// through the CLDR table, an IANA name (`Region/City`) is used as-is, and anything else
/// (a legacy `tzone://Microsoft/Custom`, or an unknown name) is preserved as a custom
/// zone rather than guessed (`calendar-semantics.md`).
fn resolve_zone(name: &str) -> Result<TimeZoneId, GraphError> {
    if let Some(iana) = windows_to_iana(name) {
        return TimeZoneId::iana(iana)
            .map_err(|e| GraphError::protocol(format!("bad IANA zone {iana:?}: {e}")));
    }
    if name.contains('/') && !name.starts_with("tzone:") {
        return TimeZoneId::iana(name)
            .map_err(|e| GraphError::protocol(format!("bad IANA zone {name:?}: {e}")));
    }
    TimeZoneId::custom(name)
        .map_err(|e| GraphError::protocol(format!("bad custom zone {name:?}: {e}")))
}

/// The event description: the plain-text `body` when Graph sent text, else the
/// server-computed `bodyPreview`.
fn description(value: &Value) -> Option<String> {
    let body = value.get("body");
    let text = if body.and_then(|b| opt_str(b, "contentType")) == Some("text") {
        body.and_then(|b| opt_str(b, "content"))
    } else {
        None
    };
    text.or_else(|| opt_str(value, "bodyPreview"))
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
}

/// The organizer (role owner) plus every attendee.
fn participants(value: &Value) -> Vec<Participant> {
    let mut out = Vec::new();
    if let Some(email) = value
        .get("organizer")
        .and_then(|o| o.get("emailAddress"))
        .and_then(address)
    {
        let mut organizer = Participant::attendee(&email.0);
        organizer.name = email.1;
        organizer.roles = [ParticipantRole::Owner].into_iter().collect();
        organizer.participation_status = ParticipationStatus::Accepted;
        organizer.expect_reply = false;
        out.push(organizer);
    }
    for attendee in value
        .get("attendees")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(email) = attendee.get("emailAddress").and_then(address) {
            out.push(attendee_from_json(attendee, email));
        }
    }
    out
}

/// One Graph attendee → a [`Participant`], mapping the `type` to a role and the
/// `status.response` to a participation status.
fn attendee_from_json(attendee: &Value, email: (String, Option<String>)) -> Participant {
    let mut participant = Participant::attendee(&email.0);
    participant.name = email.1;
    participant.roles = match opt_str(attendee, "type") {
        Some("optional") => [ParticipantRole::Optional].into_iter().collect(),
        _ => [ParticipantRole::Attendee].into_iter().collect(),
    };
    if opt_str(attendee, "type") == Some("resource") {
        participant.kind = Some(ParticipantKind::Resource);
    }
    participant.participation_status =
        match attendee.get("status").and_then(|s| opt_str(s, "response")) {
            Some("accepted" | "organizer") => ParticipationStatus::Accepted,
            Some("declined") => ParticipationStatus::Declined,
            Some("tentativelyAccepted") => ParticipationStatus::Tentative,
            _ => ParticipationStatus::NeedsAction,
        };
    participant
}

/// A Graph `emailAddress` object → `(address, name?)`, or `None` without an address.
fn address(email: &Value) -> Option<(String, Option<String>)> {
    let addr = opt_str(email, "address").filter(|a| !a.is_empty())?;
    Some((addr.to_owned(), opt_str(email, "name").map(str::to_owned)))
}

/// The physical `locations` (falling back to the singular `location`).
fn locations(value: &Value) -> Vec<Location> {
    let array = value.get("locations").and_then(Value::as_array);
    let items: Vec<&Value> = match array {
        Some(list) if !list.is_empty() => list.iter().collect(),
        _ => value.get("location").into_iter().collect(),
    };
    items
        .into_iter()
        .filter_map(|loc| {
            let name = opt_str(loc, "displayName").filter(|n| !n.is_empty())?;
            let mut location = Location::named(name);
            location.coordinates = coordinates(loc);
            Some(location)
        })
        .collect()
}

/// A `geo:` URI from a location's `latitude`/`longitude`, if both are present.
fn coordinates(loc: &Value) -> Option<String> {
    let coords = loc.get("coordinates")?;
    let lat = coords.get("latitude").and_then(Value::as_f64)?;
    let lon = coords.get("longitude").and_then(Value::as_f64)?;
    Some(format!("geo:{lat},{lon}"))
}

/// The online-meeting join URL as a virtual location, when the event is an online meeting.
fn virtual_locations(value: &Value) -> Vec<VirtualLocation> {
    value
        .get("onlineMeeting")
        .and_then(|m| opt_str(m, "joinUrl"))
        .filter(|u| !u.is_empty())
        .map(|uri| vec![VirtualLocation::new(uri)])
        .unwrap_or_default()
}

/// The revision tokens Graph supplies: the `@odata.etag` and the `changeKey`.
fn revisions(value: &Value) -> RevisionTokens {
    RevisionTokens {
        etag: opt_str(value, "@odata.etag").map(ETag::new),
        change_key: opt_str(value, "changeKey").map(ChangeKey::new),
        ..RevisionTokens::none()
    }
}

#[cfg(test)]
#[path = "cal_normalize_tests.rs"]
mod tests;
