//! Normalizing JMAP `Calendar` and `CalendarEvent` (JSCalendar, RFC 8984 / the
//! JMAP Calendars draft) into the engine's calendar model.
//!
//! The JSCalendar projection is mapped field-by-field for display, search, and
//! recurrence expansion, while the **original JSCalendar payload is preserved** as
//! [`RawJsCalendar`] beside it (the projection is lossy and not round-trip
//! authoritative — `calendar-semantics.md`). The engine time model is faithful:
//! `start` + `timeZone` becomes a zoned value, `timeZone: null` with
//! `showWithoutTime` an all-day date, and `timeZone: null` otherwise a floating
//! wall-clock time. Recurrence (rule + overrides) maps into the structural
//! [`Recurrence`] the expander consumes.

use std::collections::{BTreeMap, BTreeSet};

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::calendar::{
    Calendar, Event, EventStatus, FreeBusyStatus, Frequency, Location, NDay, Participant,
    ParticipantRole, ParticipationStatus, Privacy, Recurrence, RecurrenceBound, RecurrenceOverride,
    RecurrenceRule, VirtualLocation, Weekday,
};
use engine_core::ids::{CalendarId, EventId, Uid};
use engine_core::membership::Memberships;
use engine_core::patch::PatchObject;
use engine_core::raw::RawJsCalendar;
use engine_core::time::{CalendarDate, CalendarDateTime, Duration, LocalDateTime, TimeZoneId};
use serde_json::Value;

use crate::error::JmapError;
use crate::json::{datetime, opt_str, req_str, true_keys, wrap_id};

/// Normalizes one JMAP `Calendar` object into a [`Calendar`] container.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the object lacks a usable `id` or carries an
/// unparseable `timeZone`.
pub(crate) fn calendar_from_json(value: &Value) -> Result<Calendar, JmapError> {
    let id = wrap_id(CalendarId::try_from(req_str(value, "id")?), "calendar id")?;
    let mut calendar = Calendar::new(id, opt_str(value, "name").unwrap_or_default());
    calendar.description = opt_str(value, "description").map(str::to_owned);
    calendar.color = opt_str(value, "color").map(str::to_owned);
    if let Some(order) = value.get("sortOrder").and_then(Value::as_u64) {
        calendar.sort_order = u32::try_from(order).unwrap_or(u32::MAX);
    }
    if let Some(subscribed) = value.get("isSubscribed").and_then(Value::as_bool) {
        calendar.is_subscribed = subscribed;
    }
    if let Some(default) = value.get("isDefault").and_then(Value::as_bool) {
        calendar.is_default = default;
    }
    calendar.time_zone = parse_zone(value, "timeZone")?;
    Ok(calendar)
}

/// Normalizes one JMAP `CalendarEvent` (JSCalendar) object into an [`Event`].
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] on a missing `id`/`uid`, an empty `calendarIds`,
/// or an unparseable time/duration/recurrence value.
pub(crate) fn event_from_json(value: &Value) -> Result<Event, JmapError> {
    let id = wrap_id(EventId::try_from(req_str(value, "id")?), "event id")?;
    let uid = Uid::new(req_str(value, "uid")?)
        .map_err(|e| JmapError::protocol(format!("bad event uid: {e}")))?;
    let calendar_ids = true_keys(value, "calendarIds")
        .map(|k| wrap_id(CalendarId::try_from(k), "calendar id"))
        .collect::<Result<Vec<_>, _>>()?;
    let calendars = Memberships::new(calendar_ids).map_err(|_| {
        JmapError::protocol(format!("event {} has empty calendarIds", uid.as_str()))
    })?;

    let start = parse_start(value)?;
    let mut event = Event::new(id, uid, calendars, start);

    event.title = opt_str(value, "title")
        .map(str::to_owned)
        .unwrap_or_default();
    event.description = opt_str(value, "description").map(str::to_owned);
    event.duration = parse_duration(value)?;
    if let Some(status) = opt_str(value, "status") {
        event.status = EventStatus::from_wire(status);
    }
    if let Some(free_busy) = opt_str(value, "freeBusyStatus") {
        event.free_busy_status = FreeBusyStatus::from_wire(free_busy);
    }
    if let Some(privacy) = opt_str(value, "privacy") {
        event.privacy = Privacy::from_wire(privacy);
    }
    if let Some(sequence) = value.get("sequence").and_then(Value::as_u64) {
        event.sequence = u32::try_from(sequence).unwrap_or(u32::MAX);
    }
    event.created = datetime(value, "created")?;
    event.updated = datetime(value, "updated")?;
    event.recurrence = parse_recurrence(value)?;
    event.participants = parse_participants(value);
    event.locations = parse_locations(value);
    event.virtual_locations = parse_virtual_locations(value);
    event.color = opt_str(value, "color").map(str::to_owned);
    event.keywords = string_set(value, "keywords");
    event.categories = string_set(value, "categories");

    // Preserve the provider-native JSCalendar payload beside the projection.
    let raw = serde_json::to_string(value)
        .map_err(|e| JmapError::protocol(format!("re-serialize JSCalendar: {e}")))?;
    event.raw_jscalendar = Some(RawJsCalendar::new(raw));
    Ok(event)
}

/// Resolves `start` + `timeZone` (+ `showWithoutTime`) into the engine time model.
fn parse_start(value: &Value) -> Result<CalendarDateTime, JmapError> {
    let local = parse_local(value, "start")?;
    if let Some(zone) = parse_zone(value, "timeZone")? {
        return Ok(CalendarDateTime::Zoned { local, zone });
    }
    if value.get("showWithoutTime").and_then(Value::as_bool) == Some(true) {
        let date = CalendarDate::new(local.year(), local.month(), local.day())
            .map_err(|e| JmapError::protocol(format!("bad all-day date: {e}")))?;
        return Ok(CalendarDateTime::Date(date));
    }
    Ok(CalendarDateTime::Floating(local))
}

/// Parses a JSCalendar `LocalDateTime` field via serde (`YYYY-MM-DDThh:mm:ss`).
fn parse_local(value: &Value, key: &str) -> Result<LocalDateTime, JmapError> {
    let raw = value
        .get(key)
        .ok_or_else(|| JmapError::protocol(format!("missing {key}")))?;
    serde_json::from_value(raw.clone())
        .map_err(|e| JmapError::protocol(format!("bad {key} LocalDateTime: {e}")))
}

/// Parses an IANA `timeZone` field, or `None` for absent/null.
fn parse_zone(value: &Value, key: &str) -> Result<Option<TimeZoneId>, JmapError> {
    match opt_str(value, key) {
        None => Ok(None),
        Some(zone) => TimeZoneId::iana(zone)
            .map(Some)
            .map_err(|e| JmapError::protocol(format!("bad {key}: {e}"))),
    }
}

/// Parses the `duration` (ISO 8601), defaulting to zero when absent.
fn parse_duration(value: &Value) -> Result<Duration, JmapError> {
    match value.get("duration") {
        None => Ok(Duration::ZERO),
        Some(duration) => serde_json::from_value(duration.clone())
            .map_err(|e| JmapError::protocol(format!("bad duration: {e}"))),
    }
}

/// Builds the structural recurrence from `recurrenceRule(s)` and
/// `recurrenceOverrides`, or `None` when the event is not recurring.
fn parse_recurrence(value: &Value) -> Result<Option<Recurrence>, JmapError> {
    // Stalwart emits a singular `recurrenceRule`; the JSCalendar spec uses a
    // `recurrenceRules` array. Accept either.
    let rule = if let Some(rule) = value.get("recurrenceRule") {
        Some(parse_rule(rule)?)
    } else if let Some(rule) = value
        .get("recurrenceRules")
        .and_then(Value::as_array)
        .and_then(|rules| rules.first())
    {
        Some(parse_rule(rule)?)
    } else {
        None
    };
    let overrides = parse_overrides(value)?;
    if rule.is_none() && overrides.is_empty() {
        return Ok(None);
    }
    let mut recurrence = Recurrence::default();
    recurrence.rules.extend(rule);
    recurrence.overrides = overrides;
    Ok(Some(recurrence))
}

/// Maps one JSCalendar `RecurrenceRule` object into the structural rule.
fn parse_rule(rule: &Value) -> Result<RecurrenceRule, JmapError> {
    let frequency: Frequency = rule
        .get("frequency")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| JmapError::protocol(format!("bad frequency: {e}")))?
        .ok_or_else(|| JmapError::protocol("recurrence rule has no frequency"))?;
    let mut parsed = RecurrenceRule::new(frequency);

    if let Some(interval) = rule
        .get("interval")
        .and_then(Value::as_u64)
        .and_then(|i| u32::try_from(i).ok())
        .and_then(NonZeroU32::new)
    {
        parsed.interval = interval;
    }
    if let Some(by_day) = rule.get("byDay").and_then(Value::as_array) {
        for entry in by_day {
            let day: Weekday = entry
                .get("day")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| JmapError::protocol(format!("bad byDay day: {e}")))?
                .ok_or_else(|| JmapError::protocol("byDay entry has no day"))?;
            let nth = entry
                .get("nthOfPeriod")
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .and_then(NonZeroI32::new);
            parsed.by_day.push(NDay {
                day,
                nth_of_period: nth,
            });
        }
    }
    parsed.by_month_day = int_list(rule, "byMonthDay");
    parsed.by_month = str_list(rule, "byMonth");
    if let Some(first_day) = rule
        .get("firstDayOfWeek")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| JmapError::protocol(format!("bad firstDayOfWeek: {e}")))?
    {
        parsed.first_day_of_week = first_day;
    }
    // `count` and `until` are mutually exclusive (JSCalendar §4.3.3).
    if let Some(count) = rule
        .get("count")
        .and_then(Value::as_u64)
        .and_then(|c| u32::try_from(c).ok())
        .and_then(NonZeroU32::new)
    {
        parsed.bound = RecurrenceBound::Count(count);
    } else if let Some(until) = rule.get("until") {
        let local = serde_json::from_value(until.clone())
            .map_err(|e| JmapError::protocol(format!("bad until: {e}")))?;
        parsed.bound = RecurrenceBound::Until(local);
    }
    Ok(parsed)
}

/// Maps `recurrenceOverrides` (keyed by recurrence id) into the override map.
fn parse_overrides(
    value: &Value,
) -> Result<BTreeMap<LocalDateTime, RecurrenceOverride>, JmapError> {
    let mut overrides = BTreeMap::new();
    let Some(entries) = value.get("recurrenceOverrides").and_then(Value::as_object) else {
        return Ok(overrides);
    };
    for (recurrence_id, patch) in entries {
        let rid: LocalDateTime = recurrence_id.parse().map_err(|e| {
            JmapError::protocol(format!("bad recurrence id {recurrence_id:?}: {e}"))
        })?;
        let over = if patch.get("excluded").and_then(Value::as_bool) == Some(true) {
            RecurrenceOverride::Excluded
        } else {
            // Carry the patched properties (the expander reads start/duration/
            // status/timeZone); skip JSCalendar metadata keys.
            let fields: Vec<(String, Value)> = patch
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(key, _)| !matches!(key.as_str(), "@type" | "excluded" | "updated"))
                .map(|(key, val)| (key.clone(), val.clone()))
                .collect();
            RecurrenceOverride::Patch(
                PatchObject::new(fields)
                    .map_err(|e| JmapError::protocol(format!("bad override patch: {e}")))?,
            )
        };
        overrides.insert(rid, over);
    }
    Ok(overrides)
}

/// Maps the `participants` map's values into participants.
fn parse_participants(value: &Value) -> Vec<Participant> {
    let Some(map) = value.get("participants").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.values().map(participant_from_json).collect()
}

fn participant_from_json(participant: &Value) -> Participant {
    let roles = participant
        .get("roles")
        .and_then(Value::as_object)
        .map(|roles| {
            roles
                .keys()
                .map(|role| ParticipantRole::from_wire(role))
                .collect()
        })
        .unwrap_or_default();
    let participation_status = opt_str(participant, "participationStatus").map_or(
        ParticipationStatus::NeedsAction,
        ParticipationStatus::from_wire,
    );
    Participant {
        name: opt_str(participant, "name").map(str::to_owned),
        // `calendarAddress` is a cal-address URI ("mailto:alice@…"); store the
        // bare address as the reconciliation key.
        email: opt_str(participant, "calendarAddress")
            .map(|addr| addr.strip_prefix("mailto:").unwrap_or(addr).to_owned()),
        kind: None,
        roles,
        participation_status,
        expect_reply: participant
            .get("expectReply")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        comment: None,
        sent_by: None,
    }
}

/// Maps the `locations` map's values into locations.
fn parse_locations(value: &Value) -> Vec<Location> {
    let Some(map) = value.get("locations").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.values()
        .map(|location| {
            let mut loc = Location::named(String::new());
            loc.name = opt_str(location, "name").map(str::to_owned);
            loc.description = opt_str(location, "description").map(str::to_owned);
            loc
        })
        .collect()
}

/// Maps the `virtualLocations` map's values into virtual locations (dropping any
/// without the mandatory `uri`).
fn parse_virtual_locations(value: &Value) -> Vec<VirtualLocation> {
    let Some(map) = value.get("virtualLocations").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.values()
        .filter_map(|location| {
            let uri = opt_str(location, "uri")?;
            let mut vloc = VirtualLocation::new(uri);
            vloc.name = opt_str(location, "name").map(str::to_owned);
            vloc.description = opt_str(location, "description").map(str::to_owned);
            if let Some(features) = location.get("features").and_then(Value::as_object) {
                vloc.features = features.keys().cloned().collect();
            }
            Some(vloc)
        })
        .collect()
}

/// The keys of a JSCalendar `{ value: true }` set (`keywords`, `categories`).
fn string_set(value: &Value, key: &str) -> BTreeSet<String> {
    true_keys(value, key).map(str::to_owned).collect()
}

/// A signed-int list field, dropping non-integer entries.
fn int_list(value: &Value, key: &str) -> Vec<i32> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_i64)
                .filter_map(|n| i32::try_from(n).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// A string list field.
fn str_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod tests;
