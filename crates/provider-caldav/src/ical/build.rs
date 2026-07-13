//! Building a minimal RFC 5545 `VCALENDAR`/`VEVENT` document for a CalDAV `PUT`.
//!
//! This is the create-path counterpart to the parser ([`super`]): a host
//! constructs an event through the `engine-api` facade, this builds the iCalendar
//! body, and [`EventWrite::create`](engine_provider::EventWrite) carries it verbatim
//! in the conditional `PUT` (`caldav.md`). It is deliberately small — enough for a
//! valid create (`UID`, `DTSTAMP`, UTC `DTSTART`/`DTEND`, `SUMMARY`, optional
//! `DESCRIPTION`).
//!
//! It is **only** for a create. Rebuilding a document to *update* an event would
//! delete every property this function does not emit — the `RRULE`, the attendees,
//! the alarms, the zone — so an edit goes through [`patch_event_ical`](super::patch_event_ical),
//! which changes the stored bytes in place (`calendar-semantics.md`).
//!
//! Times use the iCalendar UTC "basic" form `YYYYMMDDTHHMMSSZ`, and text is escaped
//! per RFC 5545 §3.3.11 — the exact inverse of the parser's
//! [`unescape_text`](super::unfold::unescape_text), so a built document round-trips.
//! Both are [`format`](super::format), shared with the patcher.

use engine_core::{ids::Uid, raw::RawIcal, time::UtcDateTime};

use super::format::{escape_text, format_utc, strip_control};

/// Builds a minimal RFC 5545 `VCALENDAR`/`VEVENT` document for a create `PUT`.
///
/// `uid` is the cross-system [`Uid`]; `start`/`end` are true UTC instants emitted as
/// `DTSTART`/`DTEND` in the UTC "basic" form (`YYYYMMDDTHHMMSSZ`). `DTSTAMP` is
/// derived from `start` rather than the wall clock — engine-core time types cannot
/// read the system clock, and a create needs a stable, reproducible stamp. `summary`
/// and `description` are escaped per RFC 5545 §3.3.11. The result is the body a host
/// passes to [`EventWrite::create`](engine_provider::EventWrite).
#[must_use]
pub fn build_event_ical(
    uid: &Uid,
    summary: &str,
    start: UtcDateTime,
    end: UtcDateTime,
    description: Option<&str>,
) -> RawIcal {
    let mut ical = String::new();
    ical.push_str("BEGIN:VCALENDAR\r\n");
    ical.push_str("VERSION:2.0\r\n");
    ical.push_str("PRODID:-//PIM Sync Engine//EN\r\n");
    ical.push_str("BEGIN:VEVENT\r\n");
    // The UID is an opaque identifier carried verbatim: the parser reads it without
    // unescaping, so escaping it here would break the round trip. Control characters
    // are stripped (not escaped) so they cannot inject extra content lines — a valid
    // UID has none, so a clean UID round-trips unchanged.
    push_property(&mut ical, "UID", &strip_control(uid.as_str()));
    push_property(&mut ical, "DTSTAMP", &format_utc(start));
    push_property(&mut ical, "DTSTART", &format_utc(start));
    push_property(&mut ical, "DTEND", &format_utc(end));
    push_property(&mut ical, "SUMMARY", &escape_text(summary));
    if let Some(description) = description {
        push_property(&mut ical, "DESCRIPTION", &escape_text(description));
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
        ids::{CalendarId, EventId},
        time::CalendarDateTime,
    };

    use super::{super::parse_calendar_object, *};

    fn uid() -> Uid {
        Uid::new("evt-build-1@test.local").unwrap()
    }

    fn instant(hour: u8, minute: u8) -> UtcDateTime {
        UtcDateTime::new(2026, 6, 25, hour, minute, 0).unwrap()
    }

    #[test]
    fn build_round_trips_through_the_parser() {
        // The critical invariant: a document this builds parses back through the
        // crate's own parser (the `sync_events` read path) with the right identity,
        // title, start, and an escaped description surviving intact.
        let ical = build_event_ical(
            &uid(),
            "Team sync, take 2; final",
            instant(14, 30),
            instant(15, 0),
            Some("Line one\nLine two; with, commas"),
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
    }

    #[test]
    fn the_built_summary_line_carries_the_escaped_form() {
        // Escaping itself is `format`'s (and is tested there); this is the wiring.
        let ical = build_event_ical(&uid(), "x;y,z", instant(0, 0), instant(0, 0), None);
        assert!(ical.as_str().contains("SUMMARY:x\\;y\\,z\r\n"));
        assert!(ical.as_str().contains("DTSTAMP:20260625T000000Z\r\n"));
    }

    #[test]
    fn a_uid_with_control_chars_cannot_inject_content_lines() {
        // A UID carrying CR/LF would otherwise inject extra iCalendar lines; the
        // builder strips control chars so the UID stays a single content line.
        let evil = Uid::new("evt\r\nSUMMARY:Injected\r\nX-FOO:bar").unwrap();
        let ical = build_event_ical(&evil, "Real", instant(9, 0), instant(10, 0), None);
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
