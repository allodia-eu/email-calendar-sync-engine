//! Assembling an event's recurrence from a `VEVENT`: the master's `RRULE`s and
//! `EXDATE` exclusions, plus folding a `RECURRENCE-ID` override `VEVENT` into the
//! master's overrides.
//!
//! Parsing is **best-effort** (`calendar-semantics.md`): a malformed `RRULE`,
//! `EXDATE`, or override is skipped rather than dropping the whole event. The
//! override-map key resolves a DATE-valued `EXDATE`/`RECURRENCE-ID` against the
//! series' start time-of-day, so it matches the instants a timed series generates
//! (the expander keys instances by `date + start-time`), not silently at midnight.

use engine_core::calendar::{Event, Recurrence, RecurrenceOverride};
use engine_core::patch::PatchObject;
use engine_core::time::{CalendarDateTime, Duration, LocalDateTime};
use serde_json::Value;

use super::component::Component;
use super::event::recurrence_id_of;
use super::recur::parse_rrule;
use super::unfold::unescape_text;
use super::value::{parse_calendar_date_time, parse_date_time_list, parse_duration};
use crate::error::CalDavError;

/// Builds the structural recurrence from a master's `RRULE`s and `EXDATE`
/// exclusions, or `None` when the event is not recurring.
///
/// Best-effort: a malformed `RRULE` (unknown `FREQ`, `COUNT=0`, bad `UNTIL`) or a
/// malformed/empty `EXDATE` entry is **skipped**, never failing the whole event.
pub(super) fn parse_recurrence(
    vevent: &Component,
    series_start: &CalendarDateTime,
) -> Option<Recurrence> {
    let mut recurrence = Recurrence::default();
    for rrule in vevent.all_properties("RRULE") {
        if let Ok(rule) = parse_rrule(&rrule.value) {
            recurrence.rules.push(rule);
        }
    }
    for exdate in vevent.all_properties("EXDATE") {
        for excluded in parse_date_time_list(exdate) {
            recurrence.overrides.insert(
                override_key(&excluded, series_start),
                RecurrenceOverride::Excluded,
            );
        }
    }
    if recurrence.rules.is_empty() && recurrence.overrides.is_empty() {
        return None;
    }
    Some(recurrence)
}

/// Folds a `RECURRENCE-ID` override `VEVENT` into `master`'s overrides, keyed by
/// the original instance's wall clock (RFC 5545 §3.8.4.4).
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the override lacks a `RECURRENCE-ID` or
/// carries an unparseable value.
pub(super) fn fold_override(master: &mut Event, vevent: &Component) -> Result<(), CalDavError> {
    let recurrence_id = recurrence_id_of(vevent)?
        .ok_or_else(|| CalDavError::ical("override VEVENT missing RECURRENCE-ID"))?;
    let patch = override_patch(vevent)?;
    // Key by the recurrence-id resolved against the master's start time, so a
    // DATE-valued RECURRENCE-ID against a timed series still matches the instant.
    let key = override_key(&recurrence_id, &master.start);
    master
        .recurrence
        .get_or_insert_with(Recurrence::default)
        .overrides
        .insert(key, patch);
    Ok(())
}

/// Builds the override patch (JSCalendar-keyed, the form the expander reads) from
/// a `RECURRENCE-ID` instance's moved start, length, title, and cancellation.
fn override_patch(vevent: &Component) -> Result<RecurrenceOverride, CalDavError> {
    let mut fields: Vec<(String, Value)> = Vec::new();
    if let Some(dtstart) = vevent.property("DTSTART") {
        let start = parse_calendar_date_time(dtstart)?;
        if let Some(local) = start.local() {
            fields.push(("start".to_owned(), Value::String(local.to_string())));
        }
        if let Some(zone) = start.zone() {
            fields.push((
                "timeZone".to_owned(),
                Value::String(zone.as_str().to_owned()),
            ));
        }
        if let Some(duration) = override_duration(vevent, &start)? {
            fields.push(("duration".to_owned(), Value::String(duration.to_string())));
        }
    }
    if let Some(summary) = vevent.value("SUMMARY") {
        fields.push(("title".to_owned(), Value::String(unescape_text(summary))));
    }
    if vevent
        .value("STATUS")
        .is_some_and(|status| status.eq_ignore_ascii_case("CANCELLED"))
    {
        fields.push(("status".to_owned(), Value::String("cancelled".to_owned())));
    }
    PatchObject::new(fields)
        .map(RecurrenceOverride::Patch)
        .map_err(|e| CalDavError::ical(format!("bad override patch: {e}")))
}

/// The override instance's length, if it carries a `DTEND` or `DURATION`.
fn override_duration(
    vevent: &Component,
    start: &CalendarDateTime,
) -> Result<Option<Duration>, CalDavError> {
    if let Some(dtend) = vevent.property("DTEND") {
        let end = parse_calendar_date_time(dtend)?;
        return start
            .duration_until(&end)
            .map(Some)
            .map_err(|e| CalDavError::ical(format!("bad override DTSTART/DTEND span: {e}")));
    }
    match vevent.value("DURATION") {
        Some(duration) => parse_duration(duration).map(Some),
        None => Ok(None),
    }
}

/// The override-map key for a recurrence id (the wall clock the expander keys
/// instances by).
///
/// A timed value (floating/zoned) is its own wall clock. A DATE value is resolved
/// against `series_start`'s **time-of-day** — midnight for an all-day series (so an
/// all-day instance keys at midnight), but the series' start time for a timed
/// series, so a DATE-valued `EXDATE`/`RECURRENCE-ID` against e.g. an 09:30 series
/// matches the generated 09:30 instant instead of silently keying at midnight.
fn override_key(value: &CalendarDateTime, series_start: &CalendarDateTime) -> LocalDateTime {
    match value {
        CalendarDateTime::Floating(local) | CalendarDateTime::Zoned { local, .. } => *local,
        CalendarDateTime::Date(date) => {
            let (hour, minute, second) = series_start
                .local()
                .map_or((0, 0, 0), |t| (t.hour(), t.minute(), t.second()));
            LocalDateTime::new(date.year(), date.month(), date.day(), hour, minute, second)
                .unwrap_or_else(|_| {
                    // The date is already valid; fall back to midnight (also valid)
                    // rather than panicking if the borrowed time is somehow rejected.
                    LocalDateTime::new(date.year(), date.month(), date.day(), 0, 0, 0)
                        .unwrap_or_else(|_| unreachable!("a valid date forms a valid midnight"))
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::component::parse_components;
    use super::super::event::event_from_vevent;
    use super::*;
    use engine_core::ids::{CalendarId, EventId};
    use engine_core::raw::RawIcal;

    fn vevents(text: &str) -> Vec<Component> {
        parse_components(text)
            .into_iter()
            .flat_map(|c| c.children)
            .filter(|c| c.name == "VEVENT")
            .collect()
    }

    fn parse(text: &str) -> Event {
        let vevent = &vevents(text)[0];
        event_from_vevent(
            vevent,
            EventId::try_from("/cal/x.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
            RawIcal::new("x"),
        )
        .unwrap()
    }

    #[test]
    fn override_patch_carries_the_moved_start_and_length() {
        let vevent = &vevents(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w@x\r\n\
             RECURRENCE-ID;TZID=Europe/Amsterdam:20260126T093000\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\n\
             DTEND;TZID=Europe/Amsterdam:20260126T143000\r\n\
             SUMMARY:Moved\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        )[0];
        let RecurrenceOverride::Patch(patch) = override_patch(vevent).unwrap() else {
            panic!("expected a patch");
        };
        assert_eq!(
            patch.get("start").and_then(Value::as_str),
            Some("2026-01-26T14:00:00")
        );
        assert_eq!(patch.get("duration").and_then(Value::as_str), Some("PT30M"));
        assert_eq!(patch.get("title").and_then(Value::as_str), Some("Moved"));
    }

    #[test]
    fn a_cancelled_override_patch_marks_the_instance_cancelled() {
        let vevent = &vevents(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:c@x\r\n\
             RECURRENCE-ID;TZID=Europe/Amsterdam:20260202T093000\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260202T093000\r\nSTATUS:CANCELLED\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
        )[0];
        let RecurrenceOverride::Patch(patch) = override_patch(vevent).unwrap() else {
            panic!("expected a patch");
        };
        assert_eq!(
            patch.get("status").and_then(Value::as_str),
            Some("cancelled")
        );
    }

    #[test]
    fn date_valued_exdate_excludes_an_instant_of_a_timed_series() {
        // EXDATE;VALUE=DATE against a 09:30 series must key at 09:30 (the instant
        // the expander generates), not midnight — else the exclusion silently
        // matches nothing and the "deleted" occurrence reappears.
        let event = parse(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w@x\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n\
             RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=8\r\n\
             EXDATE;VALUE=DATE:20260119\r\nSUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let recurrence = event.recurrence.as_ref().unwrap();
        assert!(recurrence.is_excluded(&"2026-01-19T09:30:00".parse().unwrap()));
        assert!(!recurrence.is_excluded(&"2026-01-19T00:00:00".parse().unwrap()));
    }

    #[test]
    fn date_valued_exdate_keys_at_midnight_for_an_all_day_series() {
        let event = parse(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a@x\r\n\
             DTSTART;VALUE=DATE:20260105\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=4\r\n\
             EXDATE;VALUE=DATE:20260119\r\nSUMMARY:All-day\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let recurrence = event.recurrence.as_ref().unwrap();
        assert!(recurrence.is_excluded(&"2026-01-19T00:00:00".parse().unwrap()));
    }

    #[test]
    fn a_malformed_rrule_degrades_to_no_recurrence_keeping_the_event() {
        // An unknown FREQ (or COUNT=0) must not drop the whole event; the event is
        // kept as a single non-recurring occurrence (`calendar-semantics.md`).
        let event = parse(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:b@x\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n\
             RRULE:FREQ=FORTNIGHTLY\r\nSUMMARY:Kept\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(event.title, "Kept");
        assert!(
            !event.is_recurring(),
            "the bad rule is dropped, the event kept"
        );
    }
}
