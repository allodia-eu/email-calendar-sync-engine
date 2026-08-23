//! Building a minimal RFC 5545 `VCALENDAR`/`VEVENT` document for a CalDAV `PUT`.
//!
//! This is how CalDAV serializes the neutral [`EventDraft`]: a host states the intent
//! through the `engine-api` facade, and the adapter renders it as the iCalendar body of a
//! conditional `PUT` (`caldav.md`). It is deliberately small — enough for a valid create
//! (`UID`, `DTSTAMP`, `DTSTART`/`DTEND`, `SUMMARY`, optional `DESCRIPTION`).
//!
//! It is **only** for a create. Rebuilding a document to *update* an event would delete
//! every property this function does not emit — the `RRULE`, the attendees, the alarms, the
//! zone — so an edit goes through [`patch_event_ical`](super::patch_event_ical), which
//! changes the stored bytes in place (`calendar-semantics.md`).
//!
//! `DTSTART`/`DTEND` are rendered in the draft's own **form** (all-day date, floating, or
//! zoned `TZID`) by [`date_time_line`](super::format::date_time_line), never resolved to a
//! UTC instant — the same rule the patcher enforces on a move, applied at birth. Text is
//! escaped per RFC 5545 §3.3.11, the exact inverse of the parser's
//! [`unescape_text`](super::unfold::unescape_text), so a built document round-trips. Both
//! are [`format`](super::format), shared with the patcher.

use engine_core::{
    calendar::{RecurrenceBound, UntilForm, format_rrule},
    raw::RawIcal,
    time::CalendarDateTime,
};
use engine_provider::{DraftRecurrence, EventDraft};

use super::format::{date_time_line, escape_text, format_utc, strip_control};
use crate::error::IcalError;

/// Builds a minimal RFC 5545 `VCALENDAR`/`VEVENT` document for a create `PUT`.
///
/// The `DTSTAMP` is the draft's caller-supplied stamp — engine-core time types cannot read
/// the system clock, so a create's stamp is stated, not sampled.
///
/// # Errors
///
/// Returns [`IcalError`] if the draft's recurrence cannot be written as an `RRULE`: a
/// non-Gregorian rule (which would silently become Gregorian), or a rule ending at a wall
/// clock on a zoned or UTC event with no resolved instant to render `UNTIL` from — see
/// [`DraftRecurrence`].
pub fn build_event_ical(draft: &EventDraft) -> Result<RawIcal, IcalError> {
    let mut ical = String::new();
    ical.push_str("BEGIN:VCALENDAR\r\n");
    ical.push_str("VERSION:2.0\r\n");
    ical.push_str("PRODID:-//PIM Sync Engine//EN\r\n");
    ical.push_str("BEGIN:VEVENT\r\n");
    // The UID is an opaque identifier carried verbatim: the parser reads it without
    // unescaping, so escaping it here would break the round trip. Control characters
    // are stripped (not escaped) so they cannot inject extra content lines — a valid
    // UID has none, so a clean UID round-trips unchanged.
    push_property(&mut ical, "UID", &strip_control(draft.uid.as_str()));
    push_property(&mut ical, "DTSTAMP", &format_utc(draft.stamp));
    ical.push_str(&date_time_line("DTSTART", &draft.start));
    ical.push_str("\r\n");
    ical.push_str(&date_time_line("DTEND", &draft.end));
    ical.push_str("\r\n");
    push_property(&mut ical, "SUMMARY", &escape_text(&draft.summary));
    if let Some(description) = &draft.description {
        push_property(&mut ical, "DESCRIPTION", &escape_text(description));
    }
    if let Some(location) = &draft.location {
        push_property(&mut ical, "LOCATION", &escape_text(location));
    }
    if let Some(recurrence) = &draft.recurrence {
        push_property(&mut ical, "RRULE", &rrule_value(recurrence, &draft.start)?);
    }
    ical.push_str("END:VEVENT\r\n");
    ical.push_str("END:VCALENDAR\r\n");
    Ok(RawIcal::new(ical))
}

/// The `RRULE` value for a draft's recurrence, rendered in the `UNTIL` form the draft's
/// own `DTSTART` requires (RFC 5545 §3.3.10).
///
/// A zoned or UTC `DTSTART` obliges `UNTIL` to be UTC, and the instant that takes is the
/// caller's to resolve — this crate has no tzdata (`DraftRecurrence`). Refusing here is
/// the point: emitting the wall clock instead would end the series on a different day for
/// every reader outside the authoring zone.
fn rrule_value(
    recurrence: &DraftRecurrence,
    start: &CalendarDateTime,
) -> Result<String, IcalError> {
    let until = match (start, &recurrence.rule.bound) {
        // No UNTIL to render at all; the form is irrelevant.
        (_, RecurrenceBound::Unbounded | RecurrenceBound::Count(_))
        | (CalendarDateTime::Floating(_), RecurrenceBound::Until(_)) => UntilForm::Floating,
        (CalendarDateTime::Date(_), RecurrenceBound::Until(_)) => UntilForm::Date,
        (CalendarDateTime::Zoned { .. }, RecurrenceBound::Until(_)) => {
            let at = recurrence.until.ok_or_else(|| {
                IcalError::new(
                    "a recurrence ending at a wall clock on a zoned event needs that clock \
                     resolved to an instant: RFC 5545 requires UNTIL in UTC when DTSTART \
                     carries a TZID, and resolving it needs tzdata this crate does not have. \
                     Build the draft with DraftRecurrence::ending_at",
                )
            })?;
            UntilForm::Utc(at)
        }
    };
    format_rrule(&recurrence.rule, until).map_err(|e| IcalError::new(e.to_string()))
}

/// Appends one `NAME:VALUE` content line, CRLF-terminated (RFC 5545 §3.1). `value`
/// is already escaped/formatted by the caller.
fn push_property(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push(':');
    out.push_str(value);
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::{CalendarId, EventId, Uid},
        time::{CalendarDate, CalendarDateTime, TimeZoneId, UtcDateTime},
    };

    use super::{super::parse_calendar_object, *};

    fn uid() -> Uid {
        Uid::new("evt-build-1@test.local").unwrap()
    }

    fn stamp() -> UtcDateTime {
        UtcDateTime::new(2026, 6, 20, 8, 0, 0).unwrap()
    }

    fn utc(hour: u8, minute: u8) -> CalendarDateTime {
        CalendarDateTime::utc(
            format!("2026-06-25T{hour:02}:{minute:02}:00")
                .parse()
                .unwrap(),
        )
    }

    fn draft(summary: &str, start: CalendarDateTime, end: CalendarDateTime) -> EventDraft {
        EventDraft::new(
            CalendarId::try_from("/cal/").unwrap(),
            uid(),
            summary,
            start,
            end,
            stamp(),
        )
    }

    #[test]
    fn build_round_trips_through_the_parser() {
        // The critical invariant: a document this builds parses back through the
        // crate's own parser (the `sync_events` read path) with the right identity,
        // title, start, and an escaped description surviving intact.
        let ical = build_event_ical(
            &draft("Team sync, take 2; final", utc(14, 30), utc(15, 0))
                .description("Line one\nLine two; with, commas")
                .location("Room 2B; the annex, upstairs"),
        )
        .unwrap();
        let event = parse_calendar_object(
            ical.as_str(),
            EventId::try_from("/cal/evt-build-1.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
        .unwrap();

        assert_eq!(event.uid, uid());
        assert_eq!(event.title, "Team sync, take 2; final");
        assert_eq!(
            event.start,
            CalendarDateTime::utc("2026-06-25T14:30:00".parse().unwrap())
        );
        assert_eq!(event.duration, "PT30M".parse().unwrap());
        assert_eq!(
            event.description.as_deref(),
            Some("Line one\nLine two; with, commas")
        );
        // The LOCATION survives the same escape/unescape inverse the DESCRIPTION does,
        // landing back in the projection the read path builds.
        assert_eq!(
            event.locations.first().and_then(|l| l.name.as_deref()),
            Some("Room 2B; the annex, upstairs")
        );
    }

    #[test]
    fn a_draft_without_a_location_emits_no_location_line() {
        // Absent stays absent — no empty LOCATION: line, which a reader would take as a
        // location named the empty string.
        let ical = build_event_ical(&draft("No place", utc(9, 0), utc(10, 0))).unwrap();
        assert!(!ical.as_str().contains("LOCATION"), "{}", ical.as_str());
    }

    #[test]
    fn a_zoned_draft_is_born_zoned_not_resolved_to_an_instant() {
        // The same rule the patcher enforces on a *move*, applied at creation: a create in
        // Europe/Amsterdam must emit `DTSTART;TZID=…` with the wall clock, not the UTC
        // instant it happens to denote today. Flattening it here would silently re-time the
        // event the next time the zone crosses a DST boundary, and show the wrong hour to
        // every reader in another zone.
        let amsterdam = CalendarDateTime::Zoned {
            local: "2026-06-25T14:30:00".parse().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        let end = CalendarDateTime::Zoned {
            local: "2026-06-25T15:00:00".parse().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        let ical = build_event_ical(&draft("Standup", amsterdam.clone(), end)).unwrap();
        let body = ical.as_str();

        assert!(
            body.contains("DTSTART;TZID=Europe/Amsterdam:20260625T143000\r\n"),
            "{body}"
        );
        assert!(
            !body.contains("DTSTART:20260625T123000Z"),
            "the zoned start must not be flattened to its UTC instant: {body}"
        );

        let event = parse_calendar_object(
            body,
            EventId::try_from("/cal/evt-build-1.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
        .unwrap();
        assert_eq!(event.start, amsterdam);
    }

    #[test]
    fn an_all_day_draft_is_born_a_date() {
        // An all-day event is a DATE, not midnight-in-some-zone. Its DTEND is exclusive
        // (RFC 5545 §3.6.1), which the neutral draft states and this renders verbatim.
        let ical = build_event_ical(&draft(
            "Company offsite",
            CalendarDateTime::Date(CalendarDate::new(2026, 6, 25).unwrap()),
            CalendarDateTime::Date(CalendarDate::new(2026, 6, 26).unwrap()),
        ))
        .unwrap();
        let body = ical.as_str();
        assert!(body.contains("DTSTART;VALUE=DATE:20260625\r\n"), "{body}");
        assert!(body.contains("DTEND;VALUE=DATE:20260626\r\n"), "{body}");
    }

    // -----------------------------------------------------------------------
    // Recurrence
    // -----------------------------------------------------------------------

    fn weekly_on_monday() -> engine_core::calendar::RecurrenceRule {
        let mut rule =
            engine_core::calendar::RecurrenceRule::new(engine_core::calendar::Frequency::Weekly);
        rule.by_day = vec![engine_core::calendar::NDay {
            day: engine_core::calendar::Weekday::Mo,
            nth_of_period: None,
        }];
        rule
    }

    #[test]
    fn a_repeating_draft_carries_an_rrule_the_parser_reads_back() {
        let ical = build_event_ical(
            &draft("Standup", utc(9, 30), utc(10, 0))
                .repeating(DraftRecurrence::new(weekly_on_monday())),
        )
        .unwrap();
        assert!(
            ical.as_str().contains("RRULE:FREQ=WEEKLY;BYDAY=MO\r\n"),
            "{}",
            ical.as_str()
        );

        // The whole point of a create: what we wrote is what the read path gets back.
        let event = parse_calendar_object(
            ical.as_str(),
            EventId::try_from("/cal/evt-build-1.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
        .unwrap();
        assert_eq!(
            event.recurrence.as_ref().unwrap().rules,
            vec![weekly_on_monday()]
        );
    }

    #[test]
    fn a_one_off_draft_writes_no_rrule_at_all() {
        let ical = build_event_ical(&draft("Once", utc(9, 0), utc(10, 0))).unwrap();
        assert!(!ical.as_str().contains("RRULE"));
    }

    #[test]
    fn an_all_day_series_ends_on_a_date_and_a_floating_one_on_a_wall_clock() {
        // RFC 5545 §3.3.10 ties UNTIL's value type to DTSTART's, and getting it wrong is
        // how a series ends on the wrong day.
        let mut rule = weekly_on_monday();
        rule.bound =
            engine_core::calendar::RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());

        let all_day = build_event_ical(
            &draft(
                "Offsite",
                CalendarDateTime::Date(CalendarDate::new(2026, 6, 25).unwrap()),
                CalendarDateTime::Date(CalendarDate::new(2026, 6, 26).unwrap()),
            )
            .repeating(DraftRecurrence::new(rule.clone())),
        )
        .unwrap();
        assert!(
            all_day
                .as_str()
                .contains("RRULE:FREQ=WEEKLY;UNTIL=20261026;BYDAY=MO\r\n"),
            "{}",
            all_day.as_str()
        );

        let floating_start = CalendarDateTime::Floating("2026-06-25T09:00:00".parse().unwrap());
        let floating_end = CalendarDateTime::Floating("2026-06-25T10:00:00".parse().unwrap());
        let floating = build_event_ical(
            &draft("Floating", floating_start, floating_end)
                .repeating(DraftRecurrence::new(rule.clone())),
        )
        .unwrap();
        assert!(
            floating
                .as_str()
                .contains("RRULE:FREQ=WEEKLY;UNTIL=20261026T235959;BYDAY=MO\r\n"),
            "{}",
            floating.as_str()
        );
    }

    #[test]
    fn a_zoned_series_ending_at_a_wall_clock_needs_the_resolved_instant() {
        // The refusal that keeps a series from ending on a different day for everyone
        // outside the authoring zone. Emitting the wall clock here would be silently
        // wrong rather than loudly.
        let mut rule = weekly_on_monday();
        rule.bound =
            engine_core::calendar::RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
        let zoned_start = CalendarDateTime::Zoned {
            local: "2026-06-25T09:00:00".parse().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        let zoned_end = CalendarDateTime::Zoned {
            local: "2026-06-25T10:00:00".parse().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };

        let unresolved = build_event_ical(
            &draft("Standup", zoned_start.clone(), zoned_end.clone())
                .repeating(DraftRecurrence::new(rule.clone())),
        );
        assert!(unresolved.is_err(), "a zoned UNTIL must not be guessed");

        // 23:59:59 in Europe/Amsterdam is 22:59:59Z on that date.
        let resolved = build_event_ical(&draft("Standup", zoned_start, zoned_end).repeating(
            DraftRecurrence::ending_at(rule, UtcDateTime::new(2026, 10, 26, 22, 59, 59).unwrap()),
        ))
        .unwrap();
        assert!(
            resolved
                .as_str()
                .contains("RRULE:FREQ=WEEKLY;UNTIL=20261026T225959Z;BYDAY=MO\r\n"),
            "{}",
            resolved.as_str()
        );
    }

    #[test]
    fn a_counted_zoned_series_needs_no_instant() {
        // Only an UNTIL bound needs resolving; a COUNT has no clock in it.
        let mut rule = weekly_on_monday();
        rule.bound =
            engine_core::calendar::RecurrenceBound::Count(core::num::NonZeroU32::new(8).unwrap());
        let ical = build_event_ical(
            &draft(
                "Standup",
                CalendarDateTime::Zoned {
                    local: "2026-06-25T09:00:00".parse().unwrap(),
                    zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
                },
                CalendarDateTime::Zoned {
                    local: "2026-06-25T10:00:00".parse().unwrap(),
                    zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
                },
            )
            .repeating(DraftRecurrence::new(rule)),
        )
        .unwrap();
        assert!(ical.as_str().contains("RRULE:FREQ=WEEKLY;COUNT=8;BYDAY=MO"));
    }

    #[test]
    fn the_built_summary_line_carries_the_escaped_form() {
        // Escaping itself is `format`'s (and is tested there); this is the wiring. The
        // DTSTAMP is the draft's stated stamp, not derived from the start.
        let ical = build_event_ical(&draft("x;y,z", utc(0, 0), utc(0, 0))).unwrap();
        assert!(ical.as_str().contains("SUMMARY:x\\;y\\,z\r\n"));
        assert!(ical.as_str().contains("DTSTAMP:20260620T080000Z\r\n"));
    }

    #[test]
    fn a_uid_with_control_chars_cannot_inject_content_lines() {
        // A UID carrying CR/LF would otherwise inject extra iCalendar lines; the
        // builder strips control chars so the UID stays a single content line.
        let evil = Uid::new("evt\r\nSUMMARY:Injected\r\nX-FOO:bar").unwrap();
        let mut d = draft("Real", utc(9, 0), utc(10, 0));
        d.uid = evil;
        let ical = build_event_ical(&d).unwrap();
        let body = ical.as_str();
        // The control chars are gone, so the whole UID stays one content line — the
        // injected text survives only as inert UID characters, not as new properties.
        assert!(
            body.contains("UID:evtSUMMARY:InjectedX-FOO:bar\r\n"),
            "{body}"
        );
        assert!(!body.contains("\r\nSUMMARY:Injected"), "{body}");
        assert!(!body.contains("\r\nX-FOO:bar"), "{body}");
        assert!(body.contains("SUMMARY:Real\r\n"), "{body}");
    }
}
