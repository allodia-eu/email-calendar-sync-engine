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

use engine_core::raw::RawIcal;
use engine_provider::EventDraft;

use super::format::{date_time_line, escape_text, format_utc, strip_control};

/// Builds a minimal RFC 5545 `VCALENDAR`/`VEVENT` document for a create `PUT`.
///
/// The `DTSTAMP` is the draft's caller-supplied stamp — engine-core time types cannot read
/// the system clock, so a create's stamp is stated, not sampled.
#[must_use]
pub fn build_event_ical(draft: &EventDraft) -> RawIcal {
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
    ical.push_str("END:VEVENT\r\n");
    ical.push_str("END:VCALENDAR\r\n");
    RawIcal::new(ical)
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
        );
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
        let ical = build_event_ical(&draft("No place", utc(9, 0), utc(10, 0)));
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
        let ical = build_event_ical(&draft("Standup", amsterdam.clone(), end));
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
        ));
        let body = ical.as_str();
        assert!(body.contains("DTSTART;VALUE=DATE:20260625\r\n"), "{body}");
        assert!(body.contains("DTEND;VALUE=DATE:20260626\r\n"), "{body}");
    }

    #[test]
    fn the_built_summary_line_carries_the_escaped_form() {
        // Escaping itself is `format`'s (and is tested there); this is the wiring. The
        // DTSTAMP is the draft's stated stamp, not derived from the start.
        let ical = build_event_ical(&draft("x;y,z", utc(0, 0), utc(0, 0)));
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
        let ical = build_event_ical(&d);
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
