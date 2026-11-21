//! The RFC 5322 `Date` header (§3.3 `date-time`) as a UTC instant.
//!
//! JMAP and Graph hand the engine the header already parsed (`sentAt`, `sentDateTime`), so
//! their adapters never call this. The adapters that read RFC 5322 headers themselves do:
//! IMAP's `ENVELOPE` carries the `Date` header verbatim as its first field, and Gmail's
//! `payload.headers` returns the raw value. One parser serves both, so a message read over
//! Gmail's API and over its IMAP says it was sent at the same instant.
//!
//! The grammar is `time`'s RFC 2822 parser, which takes the obsolete forms RFC 5322 §4.3 tells
//! a reader to accept: no day of the week, a two-digit year, the named zones (`GMT`, `EST`, …)
//! and comments around the parts. A value it rejects is `None`: header text is hostile input,
//! and one unreadable date must never fail the message it is on.

use engine_core::time::UtcDateTime;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc2822};

/// Parses an RFC 5322 `Date` header value into a UTC instant; `None` when it does not parse.
#[must_use]
pub fn parse(raw: &str) -> Option<UtcDateTime> {
    let utc = OffsetDateTime::parse(raw, &Rfc2822)
        .ok()?
        .to_offset(UtcOffset::UTC);
    UtcDateTime::new(
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn utc(raw: &str) -> Option<String> {
        parse(raw).map(|at| at.to_string())
    }

    #[test]
    fn an_offset_is_applied_to_reach_utc() {
        assert_eq!(
            utc("Thu, 29 Oct 2026 10:00:00 +0100").as_deref(),
            Some("2026-10-29T09:00:00Z")
        );
        // A negative offset across midnight rolls the date.
        assert_eq!(
            utc("Thu, 31 Dec 2026 22:30:00 -0200").as_deref(),
            Some("2027-01-01T00:30:00Z")
        );
    }

    #[test]
    fn the_obsolete_forms_a_reader_must_accept_are_read() {
        for (raw, at) in [
            ("29 Oct 2026 10:00:00 +0000", "2026-10-29T10:00:00Z"),
            ("Thu, 29 Oct 26 10:00:00 +0000", "2026-10-29T10:00:00Z"),
            ("Thu, 29 Oct 2026 10:00:00 GMT", "2026-10-29T10:00:00Z"),
            ("Thu, 29 Oct 2026 10:00:00 EST", "2026-10-29T15:00:00Z"),
            ("Thu, 29 Oct 2026 10:00 +0000", "2026-10-29T10:00:00Z"),
            (
                "Thu, 29 Oct 2026 10:00:00 +0000 (UTC)",
                "2026-10-29T10:00:00Z",
            ),
            (
                "  Thu, 29 Oct 2026 10:00:00 +0000  ",
                "2026-10-29T10:00:00Z",
            ),
        ] {
            assert_eq!(utc(raw).as_deref(), Some(at), "{raw}");
        }
    }

    #[test]
    fn a_value_that_is_not_a_date_is_none() {
        for raw in [
            "",
            "not a date",
            "2026-10-29T10:00:00Z",
            "Thu, 32 Oct 2026 10:00:00 +0000",
            "Thu, 29 Oct 2026 25:00:00 +0000",
        ] {
            assert_eq!(utc(raw), None, "{raw}");
        }
    }
}
