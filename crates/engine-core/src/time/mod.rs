//! The engine time model.
//!
//! Calendar time has one model here: a wall-clock value resolved through a zone,
//! or wall-clock for floating time, or a zoneless calendar date for all-day
//! values (`calendar-semantics.md`). Scheduled times are **never** stored as a
//! pre-resolved UTC instant; UTC is just the IANA zone `Etc/UTC`
//! ([`CalendarDateTime::utc`]). This crate models and validates these values; it
//! does **not** resolve zones or expand recurrence — that needs bundled IANA
//! tzdata and lives in the store/index/adapter layers.
//!
//! Types:
//!
//! - [`CalendarDate`] — a zoneless calendar date (RFC 5545 `DATE`; all-day).
//! - [`LocalDateTime`] — wall-clock date-time with no zone (the spine type;
//!   JSCalendar `LocalDateTime`, RFC 8984 §1.4.5).
//! - [`UtcDateTime`] — a true UTC instant, for metadata timestamps only
//!   (JSCalendar `UTCDateTime`, RFC 8984 §1.4.4).
//! - [`TimeZoneId`] — an IANA zone name or a custom (embedded-VTIMEZONE) id,
//!   recording which expansion source applies.
//! - [`CalendarDateTime`] — the four-case scheduled-time value
//!   (date / floating / zoned, with UTC as zoned `Etc/UTC`).
//! - [`Duration`] / [`SignedDuration`] — nominal-days-plus-absolute-time lengths;
//!   signed only for alert offsets.
//!
//! All string types round-trip their canonical RFC form through `Display`,
//! `FromStr`, and `serde` (as a string).

use time::{Date, Month, PrimitiveDateTime, Time};

mod date;
mod datetime;
mod duration;
mod value;
mod zone;

pub use date::CalendarDate;
pub use datetime::{LocalDateTime, UtcDateTime};
pub use duration::{Duration, SignedDuration};
pub use value::CalendarDateTime;
pub use zone::TimeZoneId;

/// Error returned when constructing or parsing a time value fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TimeError {
    /// The input did not match the expected canonical form.
    #[error("malformed {expected}: {found:?}")]
    Malformed {
        /// A short description of the expected form.
        expected: &'static str,
        /// The offending input.
        found: String,
    },
    /// A date or time component was outside its valid range (e.g. month 13).
    #[error("date/time component out of range")]
    OutOfRange,
    /// The value carried sub-nanosecond fractional-second precision, which the
    /// engine does not represent.
    #[error("subsecond precision finer than nanoseconds is not supported")]
    SubsecondTooPrecise,
    /// A zone or duration value was empty.
    #[error("value must not be empty")]
    Empty,
}

/// Parses exactly `len` ASCII digits into a `u32`, rejecting signs, whitespace,
/// and non-ASCII digits (which `u32::from_str` would otherwise tolerate).
fn parse_digits(s: &str, expected: &'static str) -> Result<u32, TimeError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TimeError::Malformed {
            expected,
            found: s.to_owned(),
        });
    }
    s.parse::<u32>().map_err(|_| TimeError::OutOfRange)
}

/// Parses a `YYYY-MM-DD` calendar date.
fn parse_ymd(s: &str) -> Result<Date, TimeError> {
    let malformed = || TimeError::Malformed {
        expected: "date YYYY-MM-DD",
        found: s.to_owned(),
    };
    if s.len() != 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
        return Err(malformed());
    }
    let year = i32::try_from(parse_digits(&s[0..4], "date YYYY-MM-DD")?)
        .map_err(|_| TimeError::OutOfRange)?;
    let month = parse_digits(&s[5..7], "date YYYY-MM-DD")?;
    let day = parse_digits(&s[8..10], "date YYYY-MM-DD")?;
    let month = u8::try_from(month)
        .ok()
        .and_then(|m| Month::try_from(m).ok())
        .ok_or(TimeError::OutOfRange)?;
    let day = u8::try_from(day).map_err(|_| TimeError::OutOfRange)?;
    Date::from_calendar_date(year, month, day).map_err(|_| TimeError::OutOfRange)
}

/// Parses an `hh:mm:ss[.fff]` wall-clock time. Fractional seconds are optional,
/// non-canonical trailing zeros are tolerated (and normalized away on output),
/// and more than nine fractional digits is rejected as too precise.
fn parse_hms(s: &str) -> Result<Time, TimeError> {
    let malformed = || TimeError::Malformed {
        expected: "time hh:mm:ss[.fff]",
        found: s.to_owned(),
    };
    if s.len() < 8 || s.as_bytes()[2] != b':' || s.as_bytes()[5] != b':' {
        return Err(malformed());
    }
    let hour = parse_digits(&s[0..2], "time hh:mm:ss")?;
    let minute = parse_digits(&s[3..5], "time hh:mm:ss")?;
    let second = parse_digits(&s[6..8], "time hh:mm:ss")?;

    let nanosecond = if s.len() == 8 {
        0
    } else {
        if s.as_bytes()[8] != b'.' {
            return Err(malformed());
        }
        parse_subsecond(&s[9..])?
    };

    let (hour, minute, second) = (
        u8::try_from(hour).map_err(|_| TimeError::OutOfRange)?,
        u8::try_from(minute).map_err(|_| TimeError::OutOfRange)?,
        u8::try_from(second).map_err(|_| TimeError::OutOfRange)?,
    );
    Time::from_hms_nano(hour, minute, second, nanosecond).map_err(|_| TimeError::OutOfRange)
}

/// Converts the fractional-second digits after the decimal point into a
/// nanosecond count (0..1_000_000_000).
fn parse_subsecond(frac: &str) -> Result<u32, TimeError> {
    if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TimeError::Malformed {
            expected: "fractional seconds",
            found: frac.to_owned(),
        });
    }
    if frac.len() > 9 {
        // Tolerate trailing zeros beyond nanosecond precision, reject real ones.
        if frac[9..].bytes().any(|b| b != b'0') {
            return Err(TimeError::SubsecondTooPrecise);
        }
    }
    let take = frac.len().min(9);
    let mut nanos: u32 = frac[..take].parse().map_err(|_| TimeError::OutOfRange)?;
    for _ in take..9 {
        nanos *= 10;
    }
    Ok(nanos)
}

/// Formats a [`Date`] as `YYYY-MM-DD`.
fn format_date(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Formats a wall-clock date-time as `YYYY-MM-DDThh:mm:ss[.fff]`, omitting the
/// fractional part when zero and trimming its trailing zeros (the single
/// canonical representation required by RFC 8984 §1.4.4/§1.4.5).
fn format_wall_clock(dt: PrimitiveDateTime) -> String {
    let (date, time) = (dt.date(), dt.time());
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        date.year(),
        u8::from(date.month()),
        date.day(),
        time.hour(),
        time.minute(),
        time.second(),
    );
    let nanos = time.nanosecond();
    if nanos != 0 {
        let frac = format!("{nanos:09}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out
}

/// Parses a `YYYY-MM-DDThh:mm:ss[.fff]` wall-clock date-time (no zone/offset).
fn parse_wall_clock(s: &str) -> Result<PrimitiveDateTime, TimeError> {
    if s.len() < 19 || s.as_bytes()[10] != b'T' {
        return Err(TimeError::Malformed {
            expected: "date-time YYYY-MM-DDThh:mm:ss[.fff]",
            found: s.to_owned(),
        });
    }
    let date = parse_ymd(&s[0..10])?;
    let time = parse_hms(&s[11..])?;
    Ok(PrimitiveDateTime::new(date, time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_digits_rejects_signs_and_non_digits() {
        assert!(parse_digits("+1", "x").is_err());
        assert!(parse_digits("1a", "x").is_err());
        assert!(parse_digits("", "x").is_err());
        assert_eq!(parse_digits("042", "x").unwrap(), 42);
    }

    #[test]
    fn subsecond_parsing_scales_and_bounds() {
        assert_eq!(parse_subsecond("003").unwrap(), 3_000_000);
        assert_eq!(parse_subsecond("5").unwrap(), 500_000_000);
        assert_eq!(parse_subsecond("123456789").unwrap(), 123_456_789);
        // Trailing zeros beyond nanoseconds are tolerated...
        assert_eq!(parse_subsecond("1234567890").unwrap(), 123_456_789);
        // ...but real sub-nanosecond precision is rejected.
        assert_eq!(
            parse_subsecond("1234567891"),
            Err(TimeError::SubsecondTooPrecise)
        );
    }

    #[test]
    fn wall_clock_roundtrips_and_normalizes_fraction() {
        let dt = parse_wall_clock("2006-01-02T15:04:05.003").unwrap();
        assert_eq!(format_wall_clock(dt), "2006-01-02T15:04:05.003");
        // Trailing-zero fractions normalize away.
        let dt = parse_wall_clock("2010-10-10T10:10:10.000").unwrap();
        assert_eq!(format_wall_clock(dt), "2010-10-10T10:10:10");
    }

    #[test]
    fn ymd_rejects_impossible_dates() {
        assert!(parse_ymd("2021-02-29").is_err()); // not a leap year
        assert!(parse_ymd("2020-02-29").is_ok()); // leap year
        assert!(parse_ymd("2021-13-01").is_err());
        assert!(parse_ymd("2021-1-1").is_err()); // not zero-padded
    }
}
