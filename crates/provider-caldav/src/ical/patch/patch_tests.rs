//! Round-trip tests for the structural patcher.
//!
//! The assertion that matters is not "the new value is in the document" — that passes
//! for a patcher that silently deleted the `RRULE` on its way. It is
//! [`assert_only_changed`]: after the patch, **every logical line the patch did not
//! target is byte-identical**, folding and terminators included. A patcher without that
//! test is a data-loss bug with a green suite.

use engine_core::{
    ids::{CalendarId, EventId},
    raw::RawIcal,
    time::{CalendarDate, CalendarDateTime, TimeZoneId, UtcDateTime},
};

use super::{
    super::{lines::Document, parse_calendar_object},
    *,
};
use crate::error::CalDavError;

/// A resource as a real server hands it back: a zoned weekly series with an `EXDATE`,
/// three attendees (one folded across physical lines, with a `DQUOTE`-quoted `CN`
/// containing a comma), a long folded non-ASCII `DESCRIPTION`, `X-` properties nothing
/// in the projection models, an embedded `VTIMEZONE`, and a `VALARM` — whose own
/// `SUMMARY` and `DESCRIPTION` must never be mistaken for the event's.
const SERIES: &str = concat!(
    "BEGIN:VCALENDAR\r\n",
    "VERSION:2.0\r\n",
    "PRODID:-//Example Corp//CalDAV Client//NL\r\n",
    "CALSCALE:GREGORIAN\r\n",
    "BEGIN:VTIMEZONE\r\n",
    "TZID:Europe/Amsterdam\r\n",
    "BEGIN:DAYLIGHT\r\n",
    "TZOFFSETFROM:+0100\r\n",
    "TZOFFSETTO:+0200\r\n",
    "TZNAME:CEST\r\n",
    "DTSTART:19700329T020000\r\n",
    "RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\n",
    "END:DAYLIGHT\r\n",
    "BEGIN:STANDARD\r\n",
    "TZOFFSETFROM:+0200\r\n",
    "TZOFFSETTO:+0100\r\n",
    "TZNAME:CET\r\n",
    "DTSTART:19701025T030000\r\n",
    "RRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\n",
    "END:STANDARD\r\n",
    "END:VTIMEZONE\r\n",
    "BEGIN:VEVENT\r\n",
    "UID:standup-4711@example.com\r\n",
    "DTSTAMP:20260101T080000Z\r\n",
    "CREATED:20251201T093000Z\r\n",
    "LAST-MODIFIED:20260101T080000Z\r\n",
    "SEQUENCE:3\r\n",
    "DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n",
    "DTEND;TZID=Europe/Amsterdam:20260105T100000\r\n",
    "RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=20\r\n",
    "EXDATE;TZID=Europe/Amsterdam:20260119T093000\r\n",
    "SUMMARY:Sprintplanning — Zürich\r\n",
    "LOCATION:Vergaderzaal 3\\, tweede verdieping\r\n",
    "DESCRIPTION:Wekelijkse sprintplanning met het hele team. Neem je notities m\r\n",
    " ee\\, en denk aan de retrospectieve — we sluiten af met de demo.\r\n",
    "CLASS:PRIVATE\r\n",
    "TRANSP:OPAQUE\r\n",
    "CATEGORIES:WERK,SPRINT\r\n",
    "X-MICROSOFT-CDO-BUSYSTATUS:BUSY\r\n",
    "X-EXAMPLE-ROOM-BOOKING:room-3;confirmed\r\n",
    "ORGANIZER;CN=Baas:mailto:baas@example.com\r\n",
    "ATTENDEE;CN=Baas;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:baas@example.com\r\n",
    "ATTENDEE;CN=\"van der Berg, Jan\";ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;RS\r\n",
    " VP=TRUE:mailto:jan@example.com\r\n",
    "ATTENDEE;CN=Ik;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION:mailto:ik@exampl\r\n",
    " e.com\r\n",
    "BEGIN:VALARM\r\n",
    "ACTION:DISPLAY\r\n",
    "TRIGGER:-PT15M\r\n",
    "SUMMARY:Herinnering\r\n",
    "DESCRIPTION:Sprintplanning begint zo\r\n",
    "END:VALARM\r\n",
    "END:VEVENT\r\n",
    "END:VCALENDAR\r\n",
);

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 2, 10, 11, 30, 0).unwrap()
}

fn patch() -> EventPatch {
    EventPatch::new(stamp())
}

fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn apply(ical: &str, target: &PatchTarget, patch: &EventPatch) -> String {
    patch_event_ical(&RawIcal::new(ical), target, patch)
        .unwrap()
        .as_str()
        .to_owned()
}

/// Asserts that `after` differs from `before` in **only** the logical lines whose
/// property name is in `changed` — every other byte, including the original folding and
/// line terminators, is identical.
///
/// This is the whole point of a structural patcher, so it is checked structurally:
/// strike the named properties out of both documents and what remains must be equal.
fn assert_only_changed(before: &str, after: &str, changed: &[&str]) {
    let strike = |text: &str| {
        let doc = Document::parse(text);
        (0..doc.len())
            .filter(|&group| {
                let logical = doc.logical(group);
                let name = super::vevent::property_name(&logical);
                !changed
                    .iter()
                    .any(|target| name.eq_ignore_ascii_case(target))
            })
            .map(|group| doc.render_range(group..group + 1, &super::super::lines::Edits::new()))
            .collect::<String>()
    };
    assert_eq!(
        strike(before),
        strike(after),
        "a line outside {changed:?} changed"
    );
}

fn reparse(text: &str) -> engine_core::calendar::Event {
    parse_calendar_object(
        text,
        EventId::try_from("/cal/standup.ics").unwrap(),
        CalendarId::try_from("/cal/").unwrap(),
    )
    .unwrap()
}

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
    assert!(matches!(err, CalDavError::Ical(_)));

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
    assert!(matches!(err, CalDavError::Ical(_)));
}

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
    assert!(matches!(err, CalDavError::Ical(_)));
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
    assert!(matches!(err, CalDavError::Ical(_)));

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
    assert!(matches!(
        patch_event_ical(
            &RawIcal::new(no_start),
            &PatchTarget::Series,
            &patch().summary("x"),
        ),
        Err(CalDavError::Ical(_))
    ));
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
            &PatchTarget::Instance(amsterdam("2026-01-05T09:30:00")),
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
