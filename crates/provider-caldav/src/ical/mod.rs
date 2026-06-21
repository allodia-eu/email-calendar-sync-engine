//! The iCalendar (RFC 5545) parser: a calendar object resource → a normalized
//! [`Event`].
//!
//! A CalDAV calendar object resource is one `VCALENDAR` whose `VEVENT`s all share
//! a `UID` (RFC 4791 §4.1): a series **master** plus its `RECURRENCE-ID`
//! overrides. This crate folds them into a *single* [`Event`] — the master
//! carrying its overrides inline — exactly the shape the JMAP adapter produces
//! from one JSCalendar object, so the recurrence expander and the rest of the
//! engine see one representation regardless of transport. The resource's identity
//! ([`EventId`], from its href) and calendar membership ([`CalendarId`]) are
//! supplied by the caller; the whole resource text is preserved as [`RawIcal`].

mod component;
mod event;
mod party;
mod recur;
mod recurrence;
mod unfold;
mod value;

use engine_core::calendar::Event;
use engine_core::ids::{CalendarId, EventId};
use engine_core::raw::RawIcal;

use component::{Component, parse_components};
use event::{event_from_vevent, vevent_uid};
use recurrence::fold_override;

use crate::error::CalDavError;

/// Parses one calendar object resource into a single normalized [`Event`].
///
/// The master `VEVENT` (the one without a `RECURRENCE-ID`) becomes the event; its
/// `RECURRENCE-ID` siblings are folded into the event's recurrence overrides. A
/// resource that carries only an override (no master) yields that override as a
/// standalone instance event. `id` and `calendar` come from the resource href and
/// its collection; the full `text` is preserved verbatim as [`RawIcal`].
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the resource has no `VEVENT`, or the master
/// `VEVENT` is missing a `UID`/`DTSTART` or carries an unparseable value.
pub fn parse_calendar_object(
    text: &str,
    id: EventId,
    calendar: CalendarId,
) -> Result<Event, CalDavError> {
    let roots = parse_components(text);
    let vevents = collect_vevents(&roots);
    let first = *vevents
        .first()
        .ok_or_else(|| CalDavError::ical("resource has no VEVENT"))?;

    // RFC 4791 §4.1: every component in a resource shares one UID. Keep only that
    // UID's components, so a malformed multi-UID resource cannot cross-fold. A
    // sibling whose UID cannot be read is skipped, not fatal.
    let resource_uid = vevent_uid(first)?;
    let components: Vec<&Component> = vevents
        .iter()
        .copied()
        .filter(|vevent| vevent_uid(vevent).is_ok_and(|uid| uid == resource_uid))
        .collect();

    // The series master is the component with no RECURRENCE-ID *property* (checked
    // by presence, so a present-but-unparseable RECURRENCE-ID is never mistaken for
    // a master). Fall back to the first component when there is no master (a
    // standalone override-instance resource).
    let master_pos = components
        .iter()
        .position(|vevent| vevent.property("RECURRENCE-ID").is_none());
    let representative = master_pos.unwrap_or(0);
    let mut event =
        event_from_vevent(components[representative], id, calendar, RawIcal::new(text))?;

    // Fold the override siblings only when a real master anchors the series.
    // Folding is best-effort: a malformed override is skipped, never dropping the
    // master and the rest of the series (`calendar-semantics.md`).
    if master_pos.is_some() {
        for (index, &vevent) in components.iter().enumerate() {
            if index != representative {
                let _ = fold_override(&mut event, vevent);
            }
        }
    }
    Ok(event)
}

/// Gathers every `VEVENT`, looking inside each `VCALENDAR` but also tolerating a
/// bare top-level `VEVENT`.
fn collect_vevents(roots: &[Component]) -> Vec<&Component> {
    let mut vevents = Vec::new();
    for root in roots {
        if root.name == "VEVENT" {
            vevents.push(root);
        }
        vevents.extend(root.children_named("VEVENT"));
    }
    vevents
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::calendar::{FreeBusyStatus, RecurrenceOverride};
    use engine_core::time::{CalendarDateTime, TimeZoneId};

    fn parse(text: &str) -> Event {
        parse_calendar_object(
            text,
            EventId::try_from("/cal/r.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
        .unwrap()
    }

    const ONE_OFF: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Amsterdam\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:oneoff-2001@test.local\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260318T100000\r\nDTEND;TZID=Europe/Amsterdam:20260318T110000\r\nSUMMARY:One-off zoned event\r\nLOCATION:Amsterdam HQ\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_a_zoned_one_off_event() {
        let event = parse(ONE_OFF);
        assert_eq!(event.uid.as_str(), "oneoff-2001@test.local");
        assert_eq!(event.title, "One-off zoned event");
        assert_eq!(event.duration, "PT1H".parse().unwrap());
        assert_eq!(
            event.start,
            CalendarDateTime::Zoned {
                local: "2026-03-18T10:00:00".parse().unwrap(),
                zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
            }
        );
        assert_eq!(event.locations.len(), 1);
        // The whole resource (including the VTIMEZONE) is preserved verbatim.
        assert!(
            event
                .raw_ical
                .as_ref()
                .unwrap()
                .as_str()
                .contains("VTIMEZONE")
        );
        assert!(!event.is_recurring());
    }

    const RECURRING: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:weekly-2002@test.local\r\nDTSTART;TZID=Europe/Amsterdam:20260105T093000\r\nDTEND;TZID=Europe/Amsterdam:20260105T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=8\r\nEXDATE;TZID=Europe/Amsterdam:20260119T093000\r\nSUMMARY:Weekly standup\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:weekly-2002@test.local\r\nRECURRENCE-ID;TZID=Europe/Amsterdam:20260126T093000\r\nDTSTART;TZID=Europe/Amsterdam:20260126T140000\r\nDTEND;TZID=Europe/Amsterdam:20260126T143000\r\nSUMMARY:Weekly standup (moved)\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn folds_master_and_recurrence_id_override_into_one_event() {
        let event = parse(RECURRING);
        // One event, the master, carrying the series rule.
        assert!(event.is_recurring());
        assert!(event.recurrence_id.is_none());
        let recurrence = event.recurrence.as_ref().unwrap();
        assert_eq!(recurrence.rules.len(), 1);

        // The EXDATE became an exclusion; the RECURRENCE-ID VEVENT became a patch.
        let excluded: CalendarDateTime = CalendarDateTime::Zoned {
            local: "2026-01-19T09:30:00".parse().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        assert!(recurrence.is_excluded(&excluded.local().unwrap()));
        let moved = "2026-01-26T09:30:00".parse().unwrap();
        assert!(matches!(
            recurrence.overrides.get(&moved),
            Some(RecurrenceOverride::Patch(_))
        ));
    }

    #[test]
    fn all_day_event_is_zoneless_and_transparent() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:allday-2005@test.local\r\nDTSTART;VALUE=DATE:20260401\r\nDTEND;VALUE=DATE:20260402\r\nSUMMARY:All-day\r\nTRANSP:TRANSPARENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse(text);
        assert!(event.is_all_day());
        assert!(event.start.zone().is_none());
        assert_eq!(event.free_busy_status, FreeBusyStatus::Free);
        assert_eq!(event.duration, "P1D".parse().unwrap());
    }

    #[test]
    fn a_malformed_override_does_not_drop_the_whole_series() {
        // A valid master plus an override whose RECURRENCE-ID is unparseable: the
        // master (and its rule) must survive; only the bad override is skipped.
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w@x\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n\
             RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=8\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:w@x\r\nRECURRENCE-ID;TZID=Europe/Amsterdam:garbage\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\nSUMMARY:Moved\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n";
        let event = parse(text);
        assert_eq!(event.uid.as_str(), "w@x");
        assert_eq!(event.title, "Standup");
        assert!(
            event.is_recurring(),
            "the master's rule survives the bad override"
        );
        assert!(event.recurrence_id.is_none());
    }

    #[test]
    fn a_standalone_malformed_override_resource_still_errors() {
        // With no valid master and the only VEVENT carrying an unparseable
        // RECURRENCE-ID, the resource has nothing usable → an error (skipped by the
        // sync layer), not a panic.
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w@x\r\n\
             RECURRENCE-ID;TZID=Europe/Amsterdam:garbage\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(matches!(
            parse_calendar_object(
                text,
                EventId::try_from("/cal/r.ics").unwrap(),
                CalendarId::try_from("/cal/").unwrap(),
            ),
            Err(CalDavError::Ical(_))
        ));
    }

    #[test]
    fn a_resource_without_a_vevent_is_an_error() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(matches!(
            parse_calendar_object(
                text,
                EventId::try_from("/cal/r.ics").unwrap(),
                CalendarId::try_from("/cal/").unwrap(),
            ),
            Err(CalDavError::Ical(_))
        ));
    }

    #[test]
    fn adversarial_input_does_not_panic() {
        // Truncated, mis-nested, and junk inputs must fail gracefully, never panic.
        for text in [
            "",
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT\r\nDTSTART:garbage\r\nEND:VEVENT",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:\r\nEND:VEVENT\r\nEND:VCALENDAR",
            ":::::\r\n;;;;;\r\nBEGIN\r\nEND",
        ] {
            let _ = parse_calendar_object(
                text,
                EventId::try_from("/cal/r.ics").unwrap(),
                CalendarId::try_from("/cal/").unwrap(),
            );
        }
    }
}
