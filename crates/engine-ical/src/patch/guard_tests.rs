//! What a patch **refuses**, and what it survives.
//!
//! A patcher that only ever succeeds is the dangerous kind: every rule here turns a silent
//! corruption into an error the caller can see. The edits that succeed are
//! [`patch_tests`](super::patch_tests).

use engine_core::{raw::RawIcal, time::CalendarDate};
use engine_provider::Occurrence;

use super::{test_support::*, *};

// --- the form guard: a move must never silently convert ---------------------------

#[test]
fn moving_a_zoned_event_to_a_utc_start_is_rejected() {
    // The bug this exists to prevent: a caller that resolves the event to an instant
    // and hands back UTC would move the event for every reader in another zone. It is
    // an error, not a conversion.
    let err = patch_event_ical(
        &RawIcal::new(SERIES),
        &PatchTarget::Series,
        &patch().start(CalendarDateTime::utc(
            "2026-01-05T09:00:00".parse().unwrap(),
        )),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("Europe/Amsterdam"),
        "the error should name the form it refused to change: {err}"
    );
}

#[test]
fn an_all_day_move_keeps_value_date_and_an_exclusive_end() {
    let all_day = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:vrij@example.com\r\nDTSTAMP:20260101T080000Z\r\nDTSTART;VALUE=DATE:20260401\r\nDTEND;VALUE=DATE:20260402\r\nSUMMARY:Vrije dag\r\nX-KEEP:me\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let date = |day| CalendarDateTime::Date(CalendarDate::new(2026, 4, day).unwrap());
    // Move the day from the 1st to the 8th. DTEND is exclusive, so it becomes the 9th.
    let after = apply(
        all_day,
        &PatchTarget::Series,
        &patch().start(date(8)).end(date(9)),
    );
    assert_only_changed(
        all_day,
        &after,
        &["DTSTART", "DTEND", "DTSTAMP", "SEQUENCE"],
    );
    assert!(after.contains("DTSTART;VALUE=DATE:20260408\r\n"));
    assert!(after.contains("DTEND;VALUE=DATE:20260409\r\n"));
    assert!(after.contains("X-KEEP:me\r\n"));
    // It is still a one-day, all-day event — not an instant.
    let event = reparse(&after);
    assert!(event.is_all_day());
    assert_eq!(event.duration, "P1D".parse().unwrap());

    // And turning it into a timed event is refused rather than done silently.
    assert!(
        patch_event_ical(
            &RawIcal::new(all_day),
            &PatchTarget::Series,
            &patch().start(amsterdam("2026-04-08T09:00:00")),
        )
        .is_err()
    );
}

#[test]
fn a_floating_event_stays_floating() {
    let floating = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:f@x\r\nDTSTART:20260415T120000\r\nDTEND:20260415T130000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let local = |text: &str| CalendarDateTime::Floating(text.parse().unwrap());
    let after = apply(
        floating,
        &PatchTarget::Series,
        &patch()
            .start(local("2026-04-15T14:00:00"))
            .end(local("2026-04-15T15:00:00")),
    );
    // No TZID, no trailing Z — a floating value that gained either would stop floating.
    assert!(after.contains("DTSTART:20260415T140000\r\n"), "{after}");
    assert!(
        !after.contains("DTSTART;"),
        "a floating start gained a zone: {after}"
    );
    assert!(matches!(
        reparse(&after).start,
        CalendarDateTime::Floating(_)
    ));
}

#[test]
fn an_edit_that_would_end_the_event_before_it_begins_is_refused() {
    // Moving the start past the existing end without resizing inverts the event. Neither
    // line is wrong on its own, so the check is against the end the event *will have*.
    // Left through, the reader rejects the event as malformed and drops it — the edit
    // looks saved and the event vanishes.
    let err = patch_event_ical(
        &RawIcal::new(SERIES),
        &PatchTarget::Series,
        &patch().start(amsterdam("2026-01-05T23:00:00")), // DTEND is still 10:00
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("cannot end before it begins"),
        "the error should say the edit would invert the event: {err}"
    );

    // A resize that drags the end back past the start is refused the same way.
    assert!(
        patch_event_ical(
            &RawIcal::new(SERIES),
            &PatchTarget::Series,
            &patch().end(amsterdam("2026-01-05T08:00:00")),
        )
        .is_err()
    );
    // Moving both together, keeping the event positive, is of course fine.
    let after = apply(
        SERIES,
        &PatchTarget::Series,
        &patch()
            .start(amsterdam("2026-01-05T23:00:00"))
            .end(amsterdam("2026-01-05T23:30:00")),
    );
    assert_eq!(reparse(&after).duration, "PT30M".parse().unwrap());
}

// --- ends expressed as a DURATION -------------------------------------------------

#[test]
fn a_duration_end_becomes_a_dtend_in_place() {
    // DTEND and DURATION are mutually exclusive (RFC 5545 §3.6.1) — a resize must
    // replace the DURATION, not add a second, contradictory end.
    let with_duration = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:d@x\r\nDTSTAMP:20260101T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260105T093000\r\nDURATION:PT30M\r\nSUMMARY:Kort\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let after = apply(
        with_duration,
        &PatchTarget::Series,
        &patch().end(amsterdam("2026-01-05T10:30:00")),
    );
    assert!(
        !after.contains("DURATION:"),
        "both ends were written:\n{after}"
    );
    assert!(after.contains("DTEND;TZID=Europe/Amsterdam:20260105T103000\r\n"));
    assert_eq!(reparse(&after).duration, "PT1H".parse().unwrap());
}

// --- hostile and degenerate input ---------------------------------------------------

#[test]
fn a_bare_lf_document_is_patched_without_rewriting_its_terminators() {
    let lf = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x@y\nDTSTART;TZID=Europe/Amsterdam:20260105T093000\nSUMMARY:Oud\nEND:VEVENT\nEND:VCALENDAR\n";
    let after = apply(lf, &PatchTarget::Series, &patch().summary("Nieuw"));
    assert!(!after.contains('\r'), "CRLF was forced on an LF document");
    assert!(after.contains("SUMMARY:Nieuw\n"));
    assert_only_changed(lf, &after, &["SUMMARY", "DTSTAMP"]);
}

#[test]
fn an_event_without_a_dtstart_cannot_be_patched() {
    let no_start = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\nSUMMARY:S\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    assert!(
        patch_event_ical(
            &RawIcal::new(no_start),
            &PatchTarget::Series,
            &patch().summary("x"),
        )
        .is_err()
    );
}

#[test]
fn hostile_input_errors_rather_than_panics() {
    for text in [
        "",
        "BEGIN:VCALENDAR",
        "BEGIN:VEVENT\r\nDTSTART:garbage\r\nEND:VEVENT",
        ":::::\r\n;;;;;\r\nBEGIN\r\nEND",
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:\r\nEND:VEVENT\r\nEND:VCALENDAR",
    ] {
        let _ = patch_event_ical(
            &RawIcal::new(text),
            &PatchTarget::Series,
            &patch().summary("x"),
        );
        let _ = patch_event_ical(
            &RawIcal::new(text),
            &PatchTarget::Instance(Occurrence::starting(amsterdam("2026-01-05T09:30:00"))),
            &patch().summary("x"),
        );
    }
}

#[test]
fn an_empty_patch_only_restamps() {
    let after = apply(SERIES, &PatchTarget::Series, &patch());
    assert_only_changed(SERIES, &after, &["DTSTAMP", "LAST-MODIFIED"]);
    assert!(after.contains("DTSTAMP:20260210T113000Z\r\n"));
    assert!(
        after.contains("SEQUENCE:3\r\n"),
        "an empty patch bumped SEQUENCE"
    );
}

#[test]
fn a_summary_with_ical_metacharacters_is_escaped_and_cannot_inject_a_line() {
    let after = apply(
        SERIES,
        &PatchTarget::Series,
        &patch().summary("Demo; en de retro, \r\nX-INJECTED:kwaad"),
    );
    // The newline became an escaped \n inside the SUMMARY value, not a new property.
    assert!(
        !after.contains("\r\nX-INJECTED:"),
        "a property was injected:\n{after}"
    );
    assert_eq!(
        reparse(&after).title,
        "Demo; en de retro, \nX-INJECTED:kwaad"
    );
}
