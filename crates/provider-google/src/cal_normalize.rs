//! Normalizing Google Calendar `calendarList` entries and `event` JSON into the engine
//! calendar model.
//!
//! Google events are neither iCalendar nor JSCalendar, so the projection maps every axis
//! faithfully and preserves the **raw Google event** beside it in
//! [`Event::extended`](engine_core::calendar::Event::extended) (the model invariant that
//! raw is kept beside the lossy projection — `calendar-semantics.md`). Two facts make
//! this *simpler* than Graph:
//!
//! - Event `start`/`end` are `{ dateTime, timeZone }` with an **IANA** zone (e.g.
//!   `Europe/Amsterdam`) — no Windows-zone table. The `dateTime` is RFC 3339 with the zone's
//!   offset; the wall clock (before the offset) paired with the IANA `timeZone` is the zoned value.
//!   An all-day event is `{ date }` (zoneless).
//! - Recurrence is an array of **RFC 5545 `RRULE` strings**, parsed through the shared
//!   [`engine_core::calendar::parse_rrule`] (the same parser CalDAV uses).
//!
//! Only masters (`recurrence` present) and single events are projected; a per-instance
//! override (`recurringEventId` + `originalStartTime`) is **available** but deferred
//! (cross-object master/override dedup is still staged — `calendar-semantics.md`), so the
//! fetch layer keeps masters/singles and drops `cancelled` instances as tombstones.

use engine_core::{
    calendar::{
        Calendar, CalendarAccess, Event, EventStatus, FreeBusyStatus, Location, Participant,
        ParticipantKind, ParticipantRole, ParticipationStatus, Privacy, Recurrence,
        VirtualLocation, parse_rrule,
    },
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId},
    version::{ETag, RevisionTokens},
};
use serde_json::Value;

use crate::{
    error::GoogleError,
    json::{bool_field, datetime, opt_str, req_str, wrap_id},
};

/// The namespaced key under which the whole raw Google event JSON is preserved.
const RAW_EVENT_KEY: &str = "google/event";

/// Normalizes one Google `calendarList` entry into a [`Calendar`] container.
///
/// # Errors
///
/// Returns [`GoogleError::Protocol`] if the entry lacks a usable `id`.
pub(crate) fn calendar_from_json(value: &Value) -> Result<Calendar, GoogleError> {
    let id = wrap_id(CalendarId::try_from(req_str(value, "id")?), "calendar id")?;
    let mut calendar = Calendar::new(id, opt_str(value, "summary").unwrap_or_default());
    calendar.description = opt_str(value, "description")
        .filter(|d| !d.is_empty())
        .map(str::to_owned);
    // `backgroundColor` is a concrete `#rrggbb`; `colorId` is an index into a palette.
    calendar.color = opt_str(value, "backgroundColor")
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    calendar.is_default = bool_field(value, "primary");
    // The primary calendar's id is the account's own address.
    if calendar.is_default {
        calendar.owner = opt_str(value, "id").map(str::to_owned);
    }
    calendar.time_zone = match opt_str(value, "timeZone") {
        Some(zone) => Some(
            TimeZoneId::iana(zone)
                .map_err(|e| GoogleError::protocol(format!("bad calendar zone {zone:?}: {e}")))?,
        ),
        None => None,
    };
    calendar.access = access_role(opt_str(value, "accessRole"));
    calendar.revisions = revisions(value);
    Ok(calendar)
}

/// Maps a Google `accessRole` to a [`CalendarAccess`]. `writer` grants write; `reader`
/// and `freeBusyReader` are read-only; `owner` is full.
fn access_role(role: Option<&str>) -> CalendarAccess {
    match role {
        Some("owner" | "writer") => CalendarAccess::owner(),
        _ => CalendarAccess::reader(),
    }
}

/// Normalizes one Google `event` (a master or a single instance) into an [`Event`]
/// belonging to `calendar` (the bound collection — Google event JSON does not name its
/// own calendar). `default_zone` is the calendar's zone, used when an endpoint omits its
/// own `timeZone`.
///
/// # Errors
///
/// Returns [`GoogleError::Protocol`] on a missing `id`/`iCalUID`, an unparseable
/// time/duration/recurrence, or a malformed field.
pub(crate) fn event_from_json(
    value: &Value,
    calendar: &CalendarId,
    default_zone: Option<&str>,
) -> Result<Event, GoogleError> {
    let id = wrap_id(EventId::try_from(req_str(value, "id")?), "event id")?;
    let uid_raw = opt_str(value, "iCalUID")
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| id.as_str());
    let uid =
        Uid::new(uid_raw).map_err(|e| GoogleError::protocol(format!("bad event uid: {e}")))?;

    let start = parse_endpoint(value, "start", default_zone)?;
    let end = parse_endpoint(value, "end", default_zone)?;
    let duration = start
        .duration_until(&end)
        .map_err(|e| GoogleError::protocol(format!("bad event start/end: {e}")))?;

    let mut event = Event::new(id, uid, Memberships::of_one(calendar.clone()), start);
    event.duration = duration;
    opt_str(value, "summary")
        .unwrap_or_default()
        .clone_into(&mut event.title);
    event.description = opt_str(value, "description")
        .filter(|d| !d.is_empty())
        .map(str::to_owned);
    event.status = match opt_str(value, "status") {
        Some("cancelled") => EventStatus::Cancelled,
        Some("tentative") => EventStatus::Tentative,
        _ => EventStatus::Confirmed,
    };
    // Google `transparency` defaults to `opaque` (busy); `transparent` is free.
    event.free_busy_status = match opt_str(value, "transparency") {
        Some("transparent") => FreeBusyStatus::Free,
        _ => FreeBusyStatus::Busy,
    };
    event.privacy = match opt_str(value, "visibility") {
        Some("private") => Privacy::Private,
        Some("confidential") => Privacy::Secret,
        _ => Privacy::Public,
    };
    event.created = datetime(value, "created")?;
    event.updated = datetime(value, "updated")?;
    event.recurrence = recurrence(value)?;
    event.participants = participants(value);
    event.locations = locations(value);
    event.virtual_locations = virtual_locations(value);
    event.revisions = revisions(value);
    // Preserve the provider-native payload beside the projection (Google is neither iCal
    // nor JSCalendar, so it rides `extended`, not `raw_ical`/`raw_jscalendar`).
    event.extended.set(RAW_EVENT_KEY, value.clone());
    Ok(event)
}

/// Parses a Google `start`/`end` endpoint into a [`CalendarDateTime`]: a `{ date }` is a
/// zoneless [`Date`](CalendarDateTime::Date); a `{ dateTime, timeZone }` is
/// [`Zoned`](CalendarDateTime::Zoned) with the wall clock (the RFC 3339 value stripped of
/// its offset) in the IANA `timeZone` (falling back to `default_zone`, then UTC).
fn parse_endpoint(
    value: &Value,
    key: &str,
    default_zone: Option<&str>,
) -> Result<CalendarDateTime, GoogleError> {
    let obj = value
        .get(key)
        .ok_or_else(|| GoogleError::protocol(format!("event has no {key}")))?;
    if let Some(date) = opt_str(obj, "date") {
        let parsed: CalendarDate = parse_date(date)
            .ok_or_else(|| GoogleError::protocol(format!("bad all-day {key} {date:?}")))?;
        return Ok(CalendarDateTime::Date(parsed));
    }
    let raw = req_str(obj, "dateTime")?;
    let local: LocalDateTime = strip_offset(raw)
        .parse()
        .map_err(|e| GoogleError::protocol(format!("bad {key} dateTime {raw:?}: {e}")))?;
    let zone_name = opt_str(obj, "timeZone").or(default_zone).unwrap_or("UTC");
    let zone = TimeZoneId::iana(zone_name)
        .map_err(|e| GoogleError::protocol(format!("bad IANA zone {zone_name:?}: {e}")))?;
    Ok(CalendarDateTime::Zoned { local, zone })
}

/// The wall-clock portion of an RFC 3339 date-time: everything before the timezone
/// designator (`Z`, `+hh:mm`, or `-hh:mm` after the `T`). The date's own hyphens are left
/// intact by only scanning the part after `T`.
fn strip_offset(s: &str) -> &str {
    if let Some(t) = s.find('T') {
        let time = &s[t + 1..];
        if let Some(off) = time.find(['+', '-', 'Z', 'z']) {
            return &s[..t + 1 + off];
        }
    }
    s
}

/// Parses a `YYYY-MM-DD` all-day date.
fn parse_date(s: &str) -> Option<CalendarDate> {
    let mut parts = s.split('-');
    let y = parts.next()?.parse().ok()?;
    let m = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    CalendarDate::new(y, m, d).ok()
}

/// The event's recurrence: the `RRULE` lines of the `recurrence` array parsed through the
/// shared parser (`EXRULE` → excluded rules); `None` when the event is not recurring.
/// `EXDATE`/`RDATE` are staged (per-instance overrides are deferred —
/// `calendar-semantics.md`).
fn recurrence(value: &Value) -> Result<Option<Recurrence>, GoogleError> {
    let lines = value.get("recurrence").and_then(Value::as_array);
    let Some(lines) = lines else {
        return Ok(None);
    };
    let mut recurrence = Recurrence::default();
    for line in lines.iter().filter_map(Value::as_str) {
        if let Some(rule) = line.strip_prefix("RRULE:") {
            recurrence.rules.push(
                parse_rrule(rule).map_err(|e| GoogleError::protocol(format!("bad RRULE: {e}")))?,
            );
        } else if let Some(rule) = line.strip_prefix("EXRULE:") {
            recurrence.excluded_rules.push(
                parse_rrule(rule).map_err(|e| GoogleError::protocol(format!("bad EXRULE: {e}")))?,
            );
        }
    }
    Ok((!recurrence.rules.is_empty()).then_some(recurrence))
}

/// The organizer (role owner) plus every attendee.
fn participants(value: &Value) -> Vec<Participant> {
    let mut out = Vec::new();
    if let Some((addr, name)) = value.get("organizer").and_then(address) {
        let mut organizer = Participant::attendee(&addr);
        organizer.name = name;
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
        if let Some((addr, name)) = address(attendee) {
            out.push(attendee_from_json(attendee, &addr, name));
        }
    }
    out
}

/// One Google attendee → a [`Participant`], mapping `optional`/`resource` to a role and
/// `responseStatus` to a participation status.
fn attendee_from_json(attendee: &Value, addr: &str, name: Option<String>) -> Participant {
    let mut participant = Participant::attendee(addr);
    participant.name = name;
    participant.roles = if bool_field(attendee, "optional") {
        [ParticipantRole::Optional].into_iter().collect()
    } else {
        [ParticipantRole::Attendee].into_iter().collect()
    };
    if bool_field(attendee, "resource") {
        participant.kind = Some(ParticipantKind::Resource);
    }
    participant.participation_status = match opt_str(attendee, "responseStatus") {
        Some("accepted") => ParticipationStatus::Accepted,
        Some("declined") => ParticipationStatus::Declined,
        Some("tentative") => ParticipationStatus::Tentative,
        _ => ParticipationStatus::NeedsAction,
    };
    participant
}

/// A Google person object (`{ email, displayName? }`) → `(address, name?)`, or `None`
/// without an email.
fn address(person: &Value) -> Option<(String, Option<String>)> {
    let addr = opt_str(person, "email").filter(|a| !a.is_empty())?;
    Some((
        addr.to_owned(),
        opt_str(person, "displayName").map(str::to_owned),
    ))
}

/// The event's physical location (Google's single `location` string).
fn locations(value: &Value) -> Vec<Location> {
    opt_str(value, "location")
        .filter(|l| !l.is_empty())
        .map(|name| vec![Location::named(name)])
        .unwrap_or_default()
}

/// The Meet/conference join URL as a virtual location: the `hangoutLink`, else a
/// `conferenceData.entryPoints` video URI.
fn virtual_locations(value: &Value) -> Vec<VirtualLocation> {
    let uri = opt_str(value, "hangoutLink")
        .filter(|u| !u.is_empty())
        .or_else(|| entry_point_uri(value));
    uri.map(|u| vec![VirtualLocation::new(u)])
        .unwrap_or_default()
}

/// The first `conferenceData.entryPoints` video URI, if any.
fn entry_point_uri(value: &Value) -> Option<&str> {
    value
        .get("conferenceData")
        .and_then(|c| c.get("entryPoints"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|e| opt_str(e, "entryPointType") == Some("video"))
        .and_then(|e| opt_str(e, "uri"))
        .filter(|u| !u.is_empty())
}

/// The revision tokens Google supplies: the `etag` (Google exposes no separate change
/// key).
fn revisions(value: &Value) -> RevisionTokens {
    RevisionTokens {
        etag: opt_str(value, "etag").map(ETag::new),
        ..RevisionTokens::none()
    }
}

#[cfg(test)]
#[path = "cal_normalize_tests.rs"]
mod tests;
