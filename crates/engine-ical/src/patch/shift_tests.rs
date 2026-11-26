//! Moving a series moves its overrides and exclusions with it (`SeriesShift`).
//!
//! A `RECURRENCE-ID` and an `EXDATE` name an occurrence by its **original** start (RFC 5545
//! §3.8.4.4, §3.8.5.1). Move the series and one left at the old slot names no occurrence: the
//! override is drawn as an added occurrence beside the one it was made for, which comes back,
//! and the exclusion excludes nothing, so a deleted occurrence returns.

use engine_core::calendar::RecurrenceOverride;
use engine_provider::{Occurrence, PatchTarget};

use super::test_support::*;

/// [`SERIES`] (a Monday series at 09:30 with 19 January excluded) with 26 January moved to
/// 14:00 by hand, and 2 February renamed and left at its slot.
fn overridden() -> String {
    let moved = apply(
        SERIES,
        &PatchTarget::Instance(Occurrence::starting(amsterdam("2026-01-26T09:30:00"))),
        &patch()
            .start(amsterdam("2026-01-26T14:00:00"))
            .end(amsterdam("2026-01-26T14:30:00")),
    );
    apply(
        &moved,
        &PatchTarget::Instance(Occurrence::starting(amsterdam("2026-02-02T09:30:00"))),
        &patch()
            .summary("Standup (andere naam)")
            .start(amsterdam("2026-02-02T09:30:00"))
            .end(amsterdam("2026-02-02T10:00:00")),
    )
}

#[test]
fn moving_the_series_takes_its_overrides_and_exclusions_to_their_new_slots() {
    let after = apply(
        &overridden(),
        &PatchTarget::Series,
        &patch()
            .start(amsterdam("2026-01-05T10:00:00"))
            .end(amsterdam("2026-01-05T10:30:00")),
    );

    assert!(
        after.contains("EXDATE;TZID=Europe/Amsterdam:20260119T100000\r\n"),
        "{after}"
    );
    assert!(after.contains("RECURRENCE-ID;TZID=Europe/Amsterdam:20260126T100000\r\n"));
    assert!(after.contains("RECURRENCE-ID;TZID=Europe/Amsterdam:20260202T100000\r\n"));
    // The occurrence moved by hand keeps the time it was given; the one only renamed was at its
    // slot, and moves with it, as a JSCalendar patch without a `start` would.
    assert!(after.contains("DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\n"));
    assert!(after.contains("DTSTART;TZID=Europe/Amsterdam:20260202T100000\r\n"));
    assert!(after.contains("DTEND;TZID=Europe/Amsterdam:20260202T103000\r\n"));
    assert!(
        !after.contains("T093000\r\n"),
        "nothing is left at an old slot:\n{after}"
    );

    let event = reparse(&after);
    let overrides = &event.recurrence.as_ref().expect("a series").overrides;
    let key = |text: &str| text.parse().unwrap();
    assert_eq!(
        overrides.get(&key("2026-01-19T10:00:00")),
        Some(&RecurrenceOverride::Excluded)
    );
    assert!(overrides.contains_key(&key("2026-01-26T10:00:00")));
    assert!(overrides.contains_key(&key("2026-02-02T10:00:00")));
    assert_eq!(
        overrides.len(),
        3,
        "no override is left behind: {:?}",
        overrides.keys()
    );
}

#[test]
fn a_series_edit_that_moves_no_slot_leaves_them_alone() {
    let before = overridden();
    let after = apply(&before, &PatchTarget::Series, &patch().summary("Planning"));
    assert_only_changed(&before, &after, &["SUMMARY", "DTSTAMP", "LAST-MODIFIED"]);
}
