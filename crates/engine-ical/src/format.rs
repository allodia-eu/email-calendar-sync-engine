//! Rendering engine values back into iCalendar text (RFC 5545 §3.1, §3.3).
//!
//! The write-side inverse of [`value`](super::value) (parsing) and
//! [`unfold`](super::unfold) (tokenizing): TEXT escaping, the three `DATE-TIME`
//! forms, `DATE`, and content-line folding. Both writers share it — the
//! [`build`](super::build) create-path builder and the [`patch`](super::patch)
//! structural patcher — so a document this crate writes escapes and formats the
//! same way regardless of which path wrote it, and a value survives the
//! escape→unescape round trip byte-for-byte.

use engine_core::time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime};

/// The maximum octets in one physical content line, before folding (RFC 5545 §3.1).
const FOLD_WIDTH: usize = 75;

/// Renders a `DTSTART`/`DTEND`/`RECURRENCE-ID` property as a whole logical content
/// line, in the form its value dictates — the exact inverse of
/// [`parse_calendar_date_time`](super::value::parse_calendar_date_time):
///
/// - [`CalendarDateTime::Date`] → `NAME;VALUE=DATE:YYYYMMDD` (all-day, zoneless).
/// - [`CalendarDateTime::Zoned`] in `Etc/UTC` → `NAME:YYYYMMDDThhmmssZ`.
/// - [`CalendarDateTime::Zoned`] elsewhere → `NAME;TZID=<zone>:YYYYMMDDThhmmss`.
/// - [`CalendarDateTime::Floating`] → `NAME:YYYYMMDDThhmmss` (no zone, no `Z`).
///
/// Preserving the *form* is the point: rendering a zoned or all-day value as UTC
/// would silently move the event for anyone in another zone, or turn a day into an
/// instant (`calendar-semantics.md`).
pub(crate) fn date_time_line(name: &str, value: &CalendarDateTime) -> String {
    match value {
        CalendarDateTime::Date(date) => format!("{name};VALUE=DATE:{}", format_date(*date)),
        CalendarDateTime::Floating(local) => format!("{name}:{}", format_local(*local)),
        CalendarDateTime::Zoned { local, zone } if *zone == TimeZoneId::utc() => {
            format!("{name}:{}Z", format_local(*local))
        }
        CalendarDateTime::Zoned { local, zone } => {
            format!("{name};TZID={}:{}", zone.as_str(), format_local(*local))
        }
    }
}

/// Formats a UTC instant as the iCalendar UTC "basic" form `YYYYMMDDThhmmssZ`
/// (RFC 5545 §3.3.5 form #2) — `DTSTAMP`, `LAST-MODIFIED`, `CREATED`.
pub(crate) fn format_utc(instant: UtcDateTime) -> String {
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

/// Formats a wall clock as the iCalendar "basic" local form `YYYYMMDDThhmmss`
/// (RFC 5545 §3.3.5 forms #1 and #3 — the `Z` or `TZID` is the caller's).
fn format_local(local: LocalDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second(),
    )
}

/// Formats a calendar date as the iCalendar `DATE` form `YYYYMMDD` (RFC 5545 §3.3.4).
fn format_date(date: CalendarDate) -> String {
    format!("{:04}{:02}{:02}", date.year(), date.month(), date.day())
}

/// Escapes an iCalendar TEXT value (RFC 5545 §3.3.11): `\` → `\\`, `;` → `\;`,
/// `,` → `\,`, and a newline → `\n`. The exact inverse of
/// [`unescape_text`](super::unfold::unescape_text). Any line break — `\r\n`, a lone
/// `\n`, or a lone `\r` — is normalized to a single escaped `\n`, so a break is never
/// silently dropped.
pub(crate) fn escape_text(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for ch in normalized.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Removes control characters (CR/LF/NUL and the like) from an opaque identifier so it
/// cannot inject extra iCalendar content lines. A valid UID contains none.
pub(crate) fn strip_control(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// Folds one logical content line into `≤75`-octet physical lines joined by `term` +
/// a single space (RFC 5545 §3.1), never splitting a multi-byte character, and
/// appends the trailing `term`.
///
/// Folding is not cosmetic: a server (and the next reader) round-trips the *unfolded*
/// value, so an over-long `DESCRIPTION` or `ATTENDEE` written unfolded is a malformed
/// document.
pub(crate) fn fold_line(line: &str, term: &str) -> String {
    let mut out = String::with_capacity(line.len() + term.len());
    let mut octets = 0;
    for ch in line.chars() {
        let width = ch.len_utf8();
        if octets + width > FOLD_WIDTH {
            out.push_str(term);
            out.push(' ');
            octets = 1; // the continuation's leading space
        }
        out.push(ch);
        octets += width;
    }
    out.push_str(term);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(text: &str) -> LocalDateTime {
        text.parse().unwrap()
    }

    #[test]
    fn renders_each_date_time_form() {
        // All-day stays a zoneless DATE; UTC gets a Z; a named zone keeps its TZID;
        // floating gets neither. Rendering any of these as another silently moves the
        // event.
        assert_eq!(
            date_time_line(
                "DTSTART",
                &CalendarDateTime::Date(CalendarDate::new(2026, 4, 1).unwrap())
            ),
            "DTSTART;VALUE=DATE:20260401"
        );
        assert_eq!(
            date_time_line(
                "DTSTART",
                &CalendarDateTime::utc(local("2026-03-18T10:00:00"))
            ),
            "DTSTART:20260318T100000Z"
        );
        assert_eq!(
            date_time_line(
                "DTEND",
                &CalendarDateTime::Zoned {
                    local: local("2026-03-18T11:00:00"),
                    zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
                }
            ),
            "DTEND;TZID=Europe/Amsterdam:20260318T110000"
        );
        assert_eq!(
            date_time_line(
                "DTSTART",
                &CalendarDateTime::Floating(local("2026-04-15T12:00:00"))
            ),
            "DTSTART:20260415T120000"
        );
    }

    #[test]
    fn formats_utc_in_basic_form() {
        assert_eq!(
            format_utc(UtcDateTime::new(2026, 6, 25, 9, 5, 0).unwrap()),
            "20260625T090500Z"
        );
    }

    #[test]
    fn escapes_text_special_characters() {
        // RFC 5545 §3.3.11: backslash, semicolon, comma, and newline are escaped;
        // ordinary characters pass through.
        assert_eq!(escape_text("a\\b;c,d\ne"), "a\\\\b\\;c\\,d\\ne");
        // Every line-break form normalizes to one escaped newline — never dropped.
        assert_eq!(escape_text("x\r\ny"), "x\\ny");
        assert_eq!(escape_text("x\ry"), "x\\ny");
    }

    #[test]
    fn escape_is_the_exact_inverse_of_unescape() {
        for value in ["a, b; c\nd\\e", "plain", "trailing\\", "日本語, ok"] {
            assert_eq!(
                super::super::unfold::unescape_text(&escape_text(value)),
                value
            );
        }
    }

    #[test]
    fn folds_long_lines_at_seventy_five_octets() {
        let long = format!("DESCRIPTION:{}", "x".repeat(200));
        let folded = fold_line(&long, "\r\n");
        for line in folded.trim_end().split("\r\n") {
            assert!(line.len() <= 75, "line over 75 octets: {line:?}");
        }
        // Unfolding it back (strip CRLF + the one leading space) restores the logical
        // line exactly — folding must not lose or add a byte.
        assert_eq!(folded.trim_end().replace("\r\n ", ""), long);
        assert!(folded.ends_with("\r\n"));
    }

    #[test]
    fn folding_never_splits_a_multi_byte_character() {
        // A run of 3-octet characters must fold on a character boundary, so every
        // physical line stays valid UTF-8 and ≤75 octets.
        let line = format!("SUMMARY:{}", "日".repeat(60));
        let folded = fold_line(&line, "\r\n");
        for physical in folded.trim_end().split("\r\n") {
            assert!(physical.len() <= 75, "over 75 octets: {physical:?}");
        }
        assert_eq!(folded.trim_end().replace("\r\n ", ""), line);
    }

    #[test]
    fn strips_control_characters_from_identifiers() {
        assert_eq!(strip_control("evt\r\nSUMMARY:x"), "evtSUMMARY:x");
    }
}
