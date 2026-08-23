//! What removing **one occurrence** does to the stored document, and what it refuses.
//!
//! The assertion that matters is the same one the patch tests are built on
//! ([`assert_only_changed`]): an exclusion that also disturbed the `RRULE`, the `VALARM` or
//! the folded `ATTENDEE` would still put the occurrence out of the set, and would still pass
//! a test that only looked for the `EXDATE`.

use engine_core::{
    calendar::RecurrenceOverride,
    time::{CalendarDate, CalendarDateTime},
};

use super::{super::exclude_occurrence_ical, test_support::*, *};

/// A series with one occurrence already overridden — the case where an exclusion has to
/// reach past the master's own lines.
const WITH_OVERRIDE: &str = concat!(
    "BEGIN:VCALENDAR\r\n",
    "VERSION:2.0\r\n",
    "BEGIN:VEVENT\r\n",
    "UID:standup-4711@example.com\r\n",
    "DTSTAMP:20260101T080000Z\r\n",
    "DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n",
    "DTEND;TZID=Europe/Amsterdam:20260105T100000\r\n",
    "RRULE:FREQ=WEEKLY;BYDAY=MO\r\n",
    "SUMMARY:Standup\r\n",
    "END:VEVENT\r\n",
    "BEGIN:VEVENT\r\n",
    "UID:standup-4711@example.com\r\n",
    "DTSTAMP:20260101T080000Z\r\n",
    "RECURRENCE-ID;TZID=Europe/Amsterdam:20260112T093000\r\n",
    "DTSTART;TZID=Europe/Amsterdam:20260112T140000\r\n",
    "DTEND;TZID=Europe/Amsterdam:20260112T143000\r\n",
    "SUMMARY:Moved to the afternoon\r\n",
    "END:VEVENT\r\n",
    "END:VCALENDAR\r\n",
);

fn exclude(ical: &str, occurrence: &CalendarDateTime) -> String {
    exclude_occurrence_ical(&RawIcal::new(ical), occurrence, stamp())
        .unwrap()
        .as_str()
        .to_owned()
}

#[test]
fn the_occurrence_is_excluded_and_nothing_else_moves() {
    let after = exclude(SERIES, &amsterdam("2026-02-16T09:30:00"));

    assert!(
        after.contains("EXDATE;TZID=Europe/Amsterdam:20260216T093000\r\n"),
        "the occurrence is named in the series' own form, TZID and all:\n{after}"
    );
    assert_only_changed(
        SERIES,
        &after,
        &["EXDATE", "DTSTAMP", "LAST-MODIFIED", "SEQUENCE"],
    );

    let event = reparse(&after);
    let recurrence = event.recurrence.expect("still a series");
    assert_eq!(
        recurrence
            .overrides
            .get(&"2026-02-16T09:30:00".parse().unwrap()),
        Some(&RecurrenceOverride::Excluded),
        "and the reader sees the occurrence as excluded"
    );
    assert!(!recurrence.rules.is_empty(), "the rule itself is untouched");
}

#[test]
fn an_exclusion_the_event_already_had_is_left_exactly_as_it_was() {
    // The fixture already carries `EXDATE;TZID=Europe/Amsterdam:20260119T093000`. A patcher
    // that merged the new date into that line would have to rewrite a value list and a TZID
    // this edit has nothing to say about; RFC 5545 §3.8.5.1 lets the property repeat instead.
    let after = exclude(SERIES, &amsterdam("2026-02-16T09:30:00"));

    assert!(
        after.contains("EXDATE;TZID=Europe/Amsterdam:20260119T093000\r\n"),
        "the original line survives byte for byte"
    );
    let event = reparse(&after);
    let overrides = event.recurrence.expect("a series").overrides;
    for excluded in ["2026-01-19T09:30:00", "2026-02-16T09:30:00"] {
        assert_eq!(
            overrides.get(&excluded.parse().unwrap()),
            Some(&RecurrenceOverride::Excluded),
            "both exclusions are in the set"
        );
    }
}

#[test]
fn the_override_of_the_removed_occurrence_goes_with_it() {
    // The trap: an override on an instant the rule no longer produces is not inert — the
    // expander materializes it as an *added* occurrence, so the occurrence the user just
    // deleted would keep being drawn, now at the time they had moved it to.
    let after = exclude(WITH_OVERRIDE, &amsterdam("2026-01-12T09:30:00"));

    assert!(
        !after.contains("RECURRENCE-ID"),
        "the override component is gone:\n{after}"
    );
    assert!(!after.contains("Moved to the afternoon"));
    assert_eq!(
        reparse(&after)
            .recurrence
            .expect("a series")
            .overrides
            .get(&"2026-01-12T09:30:00".parse().unwrap()),
        Some(&RecurrenceOverride::Excluded),
        "what is left is an exclusion, not a moved instance"
    );
}

#[test]
fn cancelling_an_occurrence_bumps_the_sequence() {
    // RFC 5546 §3.2.8: a change attendees must hear about revises the event. Losing an
    // occurrence is one, so it is not left to the caller to decide.
    let after = exclude(SERIES, &amsterdam("2026-02-16T09:30:00"));
    assert!(
        SERIES.contains("SEQUENCE:3\r\n") && after.contains("SEQUENCE:4\r\n"),
        "the sequence moved on:\n{after}"
    );
    assert!(after.contains("DTSTAMP:20260210T113000Z\r\n"));
    assert!(after.contains("LAST-MODIFIED:20260210T113000Z\r\n"));
}

#[test]
fn an_event_that_does_not_repeat_has_no_occurrence_to_remove() {
    let single = concat!(
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:one@example.com\r\n",
        "DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n",
        "SUMMARY:Lunch\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    );
    let error = exclude_occurrence_ical(
        &RawIcal::new(single),
        &amsterdam("2026-01-05T09:30:00"),
        stamp(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("does not recur"),
        "the caller is told to delete the event instead: {error}"
    );
}

#[test]
fn an_occurrence_named_in_another_time_form_is_refused() {
    // The series' occurrences are zoned wall clocks. An all-day value names none of them,
    // and writing it would put an EXDATE in the document that excludes nothing at all.
    let error = exclude_occurrence_ical(
        &RawIcal::new(SERIES),
        &CalendarDateTime::Date(CalendarDate::new(2026, 2, 16).unwrap()),
        stamp(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("EXDATE"),
        "the refusal names the property it would have written: {error}"
    );
}
