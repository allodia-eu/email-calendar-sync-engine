//! Building a minimal RFC 5545 `VCALENDAR`/`VEVENT` document for a CalDAV `PUT`.
//!
//! This is the create-path counterpart to the parser ([`super`]): a host
//! constructs an event through the `engine-api` facade, this builds the iCalendar
//! body, and [`EventWrite::create`](engine_provider::EventWrite) carries it verbatim
//! in the conditional `PUT` (`caldav.md`). It is deliberately small — enough for a
//! valid create (`UID`, `DTSTAMP`, UTC `DTSTART`/`DTEND`, `SUMMARY`, optional
//! `DESCRIPTION`) — **not** the full JSCalendar→iCalendar serializer, which, with a
//! structural patcher for updates, is a separate concern (`calendar-semantics.md`).
//!
//! Times use the iCalendar UTC "basic" form `YYYYMMDDTHHMMSSZ`, and text is escaped
//! per RFC 5545 §3.3.11 — the exact inverse of the parser's
//! [`unescape_text`](super::unfold::unescape_text), so a built document round-trips.

use engine_core::ids::Uid;
use engine_core::raw::RawIcal;
use engine_core::time::UtcDateTime;

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
    ical.push_str("PRODID:-//Allodia//EN\r\n");
    ical.push_str("BEGIN:VEVENT\r\n");
    // The UID is an opaque identifier carried verbatim: the parser reads it without
    // unescaping, so escaping it here would break the round trip.
    push_property(&mut ical, "UID", uid.as_str());
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

/// Formats a UTC instant as the iCalendar UTC "basic" form `YYYYMMDDTHHMMSSZ`
/// (RFC 5545 §3.3.5 form #2).
fn format_utc(instant: UtcDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        instant.year(),
        instant.month(),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second(),
    )
}

/// Escapes an iCalendar TEXT value (RFC 5545 §3.3.11): `\` → `\\`, `;` → `\;`,
/// `,` → `\,`, and a newline → `\n`. The exact inverse of
/// [`unescape_text`](super::unfold::unescape_text); a lone `\r` is folded into its
/// `\n` so `\r\n` and `\n` both serialize to a single escaped newline.
fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::parse_calendar_object;
    use super::*;
    use engine_core::ids::{CalendarId, EventId};
    use engine_core::time::CalendarDateTime;

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
    fn formats_utc_in_basic_form() {
        assert_eq!(format_utc(instant(9, 5)), "20260625T090500Z");
    }

    #[test]
    fn escapes_text_special_characters() {
        // RFC 5545 §3.3.11: backslash, semicolon, comma, and newline are escaped;
        // ordinary characters pass through.
        assert_eq!(escape_text("a\\b;c,d\ne"), "a\\\\b\\;c\\,d\\ne".to_owned());
        // A CRLF folds to a single escaped newline (the lone CR is dropped).
        assert_eq!(escape_text("x\r\ny"), "x\\ny".to_owned());
        // The built SUMMARY line carries the escaped form verbatim.
        let ical = build_event_ical(&uid(), "x;y,z", instant(0, 0), instant(0, 0), None);
        assert!(ical.as_str().contains("SUMMARY:x\\;y\\,z\r\n"));
    }
}
