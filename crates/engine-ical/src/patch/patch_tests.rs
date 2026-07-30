//! What a patch **changes**: the series, and one occurrence of it.
//!
//! The refusals — the form guard, the inversion guard, hostile input — are
//! [`guard_tests`](super::guard_tests). The fixture and the byte-equality assertion both
//! rest on are [`test_support`](super::test_support).

use super::{test_support::*, *};

// --- the series (every occurrence) ---------------------------------------------

#[test]
fn retitling_the_series_changes_only_the_summary() {
    let after = apply(SERIES, &PatchTarget::Series, &patch().summary("Standup"));
    // DTSTAMP/LAST-MODIFIED are the bookkeeping RFC 5545 requires of a revision.
    assert_only_changed(SERIES, &after, &["SUMMARY", "DTSTAMP", "LAST-MODIFIED"]);
    assert!(after.contains("SUMMARY:Standup\r\n"));
    // A retitle is not a scheduling-significant change: SEQUENCE stays put (RFC 5546
    // §3.2.8), and the RRULE, the attendees, the alarm and the X-props are all still
    // there — which the byte assertion above already proved, but state it once.
    assert!(after.contains("SEQUENCE:3\r\n"));
    assert!(after.contains("RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=20\r\n"));
    assert!(after.contains("X-EXAMPLE-ROOM-BOOKING:room-3;confirmed\r\n"));
    assert_eq!(reparse(&after).title, "Standup");
}

#[test]
fn the_alarms_summary_is_not_the_events_summary() {
    // The trap this patcher must not fall into: a VALARM has its own SUMMARY and
    // DESCRIPTION, sitting inside the VEVENT's line range.
    let after = apply(SERIES, &PatchTarget::Series, &patch().summary("Standup"));
    assert!(
        after.contains("SUMMARY:Herinnering\r\n"),
        "the alarm was retitled"
    );
    assert!(after.contains("DESCRIPTION:Sprintplanning begint zo\r\n"));
}

#[test]
fn moving_the_series_keeps_the_zone_and_bumps_the_sequence() {
    let after = apply(
        SERIES,
        &PatchTarget::Series,
        &patch()
            .start(amsterdam("2026-01-05T10:00:00"))
            .end(amsterdam("2026-01-05T10:45:00")),
    );
    assert_only_changed(
        SERIES,
        &after,
        &["DTSTART", "DTEND", "DTSTAMP", "LAST-MODIFIED", "SEQUENCE"],
    );
    // The zone survives: rendering this as UTC would move the event for everyone else.
    assert!(after.contains("DTSTART;TZID=Europe/Amsterdam:20260105T100000\r\n"));
    assert!(after.contains("DTEND;TZID=Europe/Amsterdam:20260105T104500\r\n"));
    // A move *is* significant, so attendees are told (RFC 5546 §3.2.8).
    assert!(after.contains("SEQUENCE:4\r\n"));
    assert!(after.contains("DTSTAMP:20260210T113000Z\r\n"));
    assert!(after.contains("LAST-MODIFIED:20260210T113000Z\r\n"));
    let event = reparse(&after);
    assert_eq!(event.start, amsterdam("2026-01-05T10:00:00"));
    assert_eq!(event.duration, "PT45M".parse().unwrap());
    assert!(event.is_recurring());
}

#[test]
fn a_long_non_ascii_description_is_replaced_and_refolded_not_corrupted() {
    let long = "Nieuwe agenda: eerst de demo, dan de retrospectieve — en daarna \
                bespreken we de planning voor het volgende kwartaal in Zürich.";
    let after = apply(SERIES, &PatchTarget::Series, &patch().description(long));
    assert_only_changed(SERIES, &after, &["DESCRIPTION", "DTSTAMP", "LAST-MODIFIED"]);
    for line in after.split("\r\n") {
        assert!(line.len() <= 75, "an unfolded line was written: {line:?}");
    }
    // It unfolds and unescapes back to exactly what went in — the fold did not eat a
    // byte, and the comma/em-dash survived.
    assert_eq!(reparse(&after).description.as_deref(), Some(long));
}

#[test]
fn a_description_can_be_removed() {
    let after = apply(SERIES, &PatchTarget::Series, &patch().clear_description());
    assert_only_changed(SERIES, &after, &["DESCRIPTION", "DTSTAMP", "LAST-MODIFIED"]);
    // The event's DESCRIPTION is gone; the alarm's is untouched.
    assert!(reparse(&after).description.is_none());
    assert!(after.contains("DESCRIPTION:Sprintplanning begint zo\r\n"));
}

#[test]
fn a_property_the_event_lacks_is_inserted_before_the_alarm() {
    // RFC 5545 §3.6.1 orders a VEVENT's properties ahead of its alarms, so a new
    // property must not be appended after the VALARM.
    let bare = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTART;TZID=Europe/Amsterdam:20260105T093000\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let after = apply(bare, &PatchTarget::Series, &patch().location("Zaal 1"));
    let location = after.find("LOCATION:Zaal 1").unwrap();
    let alarm = after.find("BEGIN:VALARM").unwrap();
    assert!(
        location < alarm,
        "the LOCATION landed after the VALARM:\n{after}"
    );
}

// --- one occurrence (the RECURRENCE-ID override) --------------------------------

#[test]
fn moving_one_occurrence_leaves_the_whole_original_document_byte_for_byte() {
    // The headline guarantee: dragging Monday the 26th to 14:00 must not touch the
    // series. The master's every byte — RRULE, EXDATE, attendees, alarm, X-props,
    // even its DTSTAMP — is still exactly where it was; the override is new bytes
    // spliced in before END:VCALENDAR.
    let after = apply(
        SERIES,
        &PatchTarget::Instance(amsterdam("2026-01-26T09:30:00")),
        &patch()
            .start(amsterdam("2026-01-26T14:00:00"))
            .end(amsterdam("2026-01-26T14:30:00")),
    );
    let (head, tail) = SERIES.split_once("END:VCALENDAR").unwrap();
    assert!(
        after.starts_with(head),
        "the original document changed:\n{after}"
    );
    assert!(after.ends_with(&format!("END:VCALENDAR{tail}")));

    // The override carries the occurrence's ORIGINAL start as its identity, and its
    // new start as DTSTART.
    assert!(after.contains("RECURRENCE-ID;TZID=Europe/Amsterdam:20260126T093000\r\n"));
    assert!(after.contains("DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\n"));
    assert!(after.contains("DTEND;TZID=Europe/Amsterdam:20260126T143000\r\n"));

    // It is an instance, not a second series: the rule and the exclusions did not come
    // across (RFC 5545 §3.8.5) — but the attendees, the alarm and the X-props did.
    let override_block = &after[after.find("RECURRENCE-ID").unwrap()..];
    assert!(!override_block.contains("RRULE:"));
    assert!(!override_block.contains("EXDATE"));
    assert!(override_block.contains("ATTENDEE;CN=\"van der Berg, Jan\""));
    assert!(override_block.contains("BEGIN:VALARM"));
    assert!(override_block.contains("X-EXAMPLE-ROOM-BOOKING:room-3;confirmed"));
}

#[test]
fn the_moved_occurrence_folds_back_into_the_series_on_re_read() {
    use engine_core::calendar::RecurrenceOverride;

    let after = apply(
        SERIES,
        &PatchTarget::Instance(amsterdam("2026-01-26T09:30:00")),
        &patch()
            .start(amsterdam("2026-01-26T14:00:00"))
            .end(amsterdam("2026-01-26T14:30:00")),
    );
    // End to end: the patched document parses back as the same series, still weekly,
    // now carrying a patch for that one occurrence.
    let event = reparse(&after);
    assert!(event.is_recurring());
    assert!(
        event.recurrence_id.is_none(),
        "the master is still the master"
    );
    let recurrence = event.recurrence.as_ref().unwrap();
    assert!(matches!(
        recurrence
            .overrides
            .get(&"2026-01-26T09:30:00".parse().unwrap()),
        Some(RecurrenceOverride::Patch(_))
    ));
    // And the untouched occurrence is still excluded — the EXDATE survived.
    assert!(recurrence.is_excluded(&"2026-01-19T09:30:00".parse().unwrap()));
}

#[test]
fn editing_an_occurrence_twice_patches_the_override_rather_than_adding_a_second() {
    let once = apply(
        SERIES,
        &PatchTarget::Instance(amsterdam("2026-01-26T09:30:00")),
        &patch()
            .start(amsterdam("2026-01-26T14:00:00"))
            .end(amsterdam("2026-01-26T14:30:00")),
    );
    let twice = apply(
        &once,
        &PatchTarget::Instance(amsterdam("2026-01-26T09:30:00")),
        &patch().summary("Standup (verplaatst)"),
    );
    assert_eq!(
        twice.matches("RECURRENCE-ID").count(),
        1,
        "a second override was created for the same occurrence"
    );
    // The second edit lands on the override, not the master: the series keeps its title.
    assert!(twice.contains("SUMMARY:Sprintplanning — Zürich\r\n"));
    assert!(twice.contains("SUMMARY:Standup (verplaatst)\r\n"));
    // And the first edit's move survived it.
    assert!(twice.contains("DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\n"));
    assert_only_changed(&once, &twice, &["SUMMARY", "DTSTAMP", "LAST-MODIFIED"]);
}

#[test]
fn splitting_a_new_override_demands_the_occurrences_own_times() {
    // Regression. A fresh override is copied from the master — whose DTSTART/DTEND are
    // the *first* occurrence's (5 Jan), not the one being edited (26 Jan). Patching only
    // the start once produced an override running from 26 Jan 14:00 to 5 Jan 10:00: a
    // negative duration, which the reader then silently discarded as a malformed
    // override, so the user's move vanished. Deriving the end needs the recurrence
    // expander this crate does not have, so the caller must state it.
    let err = patch_event_ical(
        &RawIcal::new(SERIES),
        &PatchTarget::Instance(amsterdam("2026-01-26T09:30:00")),
        &patch().start(amsterdam("2026-01-26T14:00:00")), // no end
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("start and end"),
        "the error should say what the caller must supply: {err}"
    );

    // Retitling one occurrence is legal — it just has to say when that occurrence is.
    let after = apply(
        SERIES,
        &PatchTarget::Instance(amsterdam("2026-01-26T09:30:00")),
        &patch()
            .start(amsterdam("2026-01-26T09:30:00"))
            .end(amsterdam("2026-01-26T10:00:00"))
            .summary("Standup (met demo)"),
    );
    let event = reparse(&after);
    assert!(event.is_recurring());
    assert!(after.contains("SUMMARY:Standup (met demo)\r\n"));
}

#[test]
fn overriding_an_instance_of_a_non_recurring_event_is_an_error() {
    let single = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTART;TZID=Europe/Amsterdam:20260105T093000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let err = patch_event_ical(
        &RawIcal::new(single),
        &PatchTarget::Instance(amsterdam("2026-01-05T09:30:00")),
        &patch().summary("x"),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("does not recur"),
        "the error should say why the instance cannot be overridden: {err}"
    );
}
