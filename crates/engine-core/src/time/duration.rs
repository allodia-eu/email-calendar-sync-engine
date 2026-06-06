//! Durations and signed durations.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use super::{TimeError, parse_subsecond};

/// A non-negative length of time (JSCalendar `Duration`, RFC 8984 §1.4.6;
/// iCalendar `DURATION`, RFC 5545 §3.3.6).
///
/// A duration separates **nominal** calendar days from **absolute** time. Adding
/// it to a wall-clock value adds the day component to the date first (a week is
/// always seven days), then the time component in absolute time — so a duration
/// spanning a DST transition is not a fixed number of seconds. The engine stores
/// the split (days vs. seconds) so the expander can honor it; it does not perform
/// the addition (that needs tzdata).
///
/// Weeks are folded into days on construction (`1W == 7D`), giving a single
/// canonical representation. The canonical string omits zero components, with
/// `PT0S` for a zero duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Duration {
    days: u64,
    seconds: u64,
    nanoseconds: u32,
}

impl Duration {
    /// A zero-length duration (`PT0S`).
    pub const ZERO: Self = Self {
        days: 0,
        seconds: 0,
        nanoseconds: 0,
    };

    /// Builds a duration from ISO-8601 components, folding weeks into days and
    /// hours/minutes/seconds into a single absolute-seconds value.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::OutOfRange`] if `nanoseconds >= 1_000_000_000` or if
    /// the components overflow `u64`.
    pub fn from_parts(
        weeks: u64,
        days: u64,
        hours: u64,
        minutes: u64,
        seconds: u64,
        nanoseconds: u32,
    ) -> Result<Self, TimeError> {
        if nanoseconds >= 1_000_000_000 {
            return Err(TimeError::OutOfRange);
        }
        let overflow = || TimeError::OutOfRange;
        let days = weeks
            .checked_mul(7)
            .and_then(|w| w.checked_add(days))
            .ok_or_else(overflow)?;
        let seconds = hours
            .checked_mul(3600)
            .and_then(|h| minutes.checked_mul(60).and_then(|m| h.checked_add(m)))
            .and_then(|hm| hm.checked_add(seconds))
            .ok_or_else(overflow)?;
        Ok(Self {
            days,
            seconds,
            nanoseconds,
        })
    }

    /// Returns the nominal day component.
    #[must_use]
    pub fn days(self) -> u64 {
        self.days
    }

    /// Returns the absolute-time component, in whole seconds.
    #[must_use]
    pub fn seconds(self) -> u64 {
        self.seconds
    }

    /// Returns the sub-second component, in nanoseconds (0..1_000_000_000).
    #[must_use]
    pub fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }

    /// Returns `true` if this is a zero-length duration.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.days == 0 && self.seconds == 0 && self.nanoseconds == 0
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return f.write_str("PT0S");
        }
        f.write_str("P")?;
        if self.days > 0 {
            write!(f, "{}D", self.days)?;
        }
        if self.seconds > 0 || self.nanoseconds > 0 {
            f.write_str("T")?;
            let (h, m, s) = (
                self.seconds / 3600,
                (self.seconds % 3600) / 60,
                self.seconds % 60,
            );
            if h > 0 {
                write!(f, "{h}H")?;
            }
            if m > 0 {
                write!(f, "{m}M")?;
            }
            if s > 0 || self.nanoseconds > 0 {
                write!(f, "{s}")?;
                if self.nanoseconds > 0 {
                    let frac = format!("{:09}", self.nanoseconds);
                    write!(f, ".{}", frac.trim_end_matches('0'))?;
                }
                f.write_str("S")?;
            }
        }
        Ok(())
    }
}

/// Parses an unsigned decimal integer, rejecting signs and non-digits.
fn parse_uint(s: &str) -> Result<u64, TimeError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TimeError::Malformed {
            expected: "duration number",
            found: s.to_owned(),
        });
    }
    s.parse().map_err(|_| TimeError::OutOfRange)
}

/// Parses the `[W][D]` date part of a duration into `(weeks, days)`, enforcing
/// that each unit appears at most once and in order.
fn parse_date_units(s: &str) -> Result<(u64, u64), TimeError> {
    let mut weeks = 0;
    let mut days = 0;
    let mut last_order = -1i32;
    let mut rest = s;
    while !rest.is_empty() {
        let (num, unit, tail) = split_unit(rest)?;
        let order = match unit {
            b'W' => 0,
            b'D' => 1,
            _ => return Err(malformed(s)),
        };
        if order <= last_order {
            return Err(malformed(s));
        }
        last_order = order;
        let value = parse_uint(num)?;
        match unit {
            b'W' => weeks = value,
            _ => days = value,
        }
        rest = tail;
    }
    Ok((weeks, days))
}

/// Parses the `[H][M][S]` time part of a duration into
/// `(hours, minutes, seconds, nanoseconds)`.
fn parse_time_units(s: &str) -> Result<(u64, u64, u64, u32), TimeError> {
    let mut hours = 0;
    let mut minutes = 0;
    let mut seconds = 0;
    let mut nanos = 0;
    let mut last_order = -1i32;
    let mut rest = s;
    while !rest.is_empty() {
        let (num, unit, tail) = split_unit(rest)?;
        let order = match unit {
            b'H' => 0,
            b'M' => 1,
            b'S' => 2,
            _ => return Err(malformed(s)),
        };
        if order <= last_order {
            return Err(malformed(s));
        }
        last_order = order;
        match unit {
            b'H' => hours = parse_uint(num)?,
            b'M' => minutes = parse_uint(num)?,
            _ => {
                let (whole, frac) = num
                    .split_once('.')
                    .map_or((num, None), |(w, f)| (w, Some(f)));
                seconds = parse_uint(whole)?;
                nanos = match frac {
                    Some(frac) => parse_subsecond(frac)?,
                    None => 0,
                };
            }
        }
        rest = tail;
    }
    Ok((hours, minutes, seconds, nanos))
}

/// Splits a leading `number + unit-letter` token off `s`, returning
/// `(number, unit, rest)`.
fn split_unit(s: &str) -> Result<(&str, u8, &str), TimeError> {
    let unit_pos = s
        .bytes()
        .position(|b| b.is_ascii_alphabetic())
        .ok_or_else(|| malformed(s))?;
    Ok((&s[..unit_pos], s.as_bytes()[unit_pos], &s[unit_pos + 1..]))
}

fn malformed(s: &str) -> TimeError {
    TimeError::Malformed {
        expected: "ISO 8601 duration",
        found: s.to_owned(),
    }
}

impl FromStr for Duration {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s.strip_prefix('P').ok_or_else(|| malformed(s))?;
        if rest.is_empty() {
            return Err(malformed(s));
        }
        let (date_part, time_part) = match rest.split_once('T') {
            Some((_, "")) => return Err(malformed(s)),
            Some((date, time)) => (date, Some(time)),
            None => (rest, None),
        };
        let (weeks, days) = parse_date_units(date_part)?;
        let (hours, minutes, seconds, nanos) = match time_part {
            Some(time) => parse_time_units(time)?,
            None => (0, 0, 0, 0),
        };
        Duration::from_parts(weeks, days, hours, minutes, seconds, nanos)
    }
}

impl TryFrom<String> for Duration {
    type Error = TimeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Duration> for String {
    fn from(value: Duration) -> Self {
        value.to_string()
    }
}

/// A signed length of time, used **only** for alert offsets (JSCalendar
/// `SignedDuration`, RFC 8984 §1.4.7).
///
/// A negative value is at or before the anchor; a positive value (the default,
/// written with no sign) is at or after it. Everything scheduling-facing uses the
/// non-negative [`Duration`] instead. A zero offset is always positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SignedDuration {
    negative: bool,
    magnitude: Duration,
}

impl SignedDuration {
    /// An at-or-after (positive) offset.
    #[must_use]
    pub fn after(magnitude: Duration) -> Self {
        Self {
            negative: false,
            magnitude,
        }
    }

    /// An at-or-before (negative) offset. A zero magnitude stays positive so
    /// there is a single representation of zero.
    #[must_use]
    pub fn before(magnitude: Duration) -> Self {
        Self {
            negative: !magnitude.is_zero(),
            magnitude,
        }
    }

    /// Returns `true` if the offset points before the anchor.
    #[must_use]
    pub fn is_before(self) -> bool {
        self.negative
    }

    /// Returns the unsigned magnitude.
    #[must_use]
    pub fn magnitude(self) -> Duration {
        self.magnitude
    }
}

impl fmt::Display for SignedDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            f.write_str("-")?;
        }
        fmt::Display::fmt(&self.magnitude, f)
    }
}

impl FromStr for SignedDuration {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (negative, body) = match s.strip_prefix(['-', '+']) {
            Some(body) => (s.starts_with('-'), body),
            None => (false, s),
        };
        let magnitude = body.parse()?;
        Ok(if negative {
            Self::before(magnitude)
        } else {
            Self::after(magnitude)
        })
    }
}

impl TryFrom<String> for SignedDuration {
    type Error = TimeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SignedDuration> for String {
    fn from(value: SignedDuration) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_folds_weeks_and_time() {
        let d = Duration::from_parts(1, 2, 3, 4, 5, 0).unwrap();
        assert_eq!(d.days(), 9); // 1 week + 2 days
        assert_eq!(d.seconds(), 3 * 3600 + 4 * 60 + 5);
        assert!(!d.is_zero());
    }

    #[test]
    fn from_parts_rejects_bad_nanoseconds() {
        assert_eq!(
            Duration::from_parts(0, 0, 0, 0, 0, 1_000_000_000),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn from_parts_rejects_overflow() {
        // Each component can overflow `u64` when folded; all map to OutOfRange.
        assert_eq!(
            Duration::from_parts(u64::MAX, 0, 0, 0, 0, 0),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Duration::from_parts(0, 0, u64::MAX, 0, 0, 0),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Duration::from_parts(0, 0, 0, u64::MAX, 0, 0),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Duration::from_parts(0, 0, 1, 0, u64::MAX, 0),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn parse_and_display_roundtrip() {
        for (text, days, seconds, nanos) in [
            ("PT0S", 0, 0, 0),
            ("P9DT3H4M5S", 9, 3 * 3600 + 4 * 60 + 5, 0),
            ("PT1H", 0, 3600, 0),
            ("PT30M", 0, 1800, 0),
            ("P7D", 7, 0, 0),
            ("PT0.5S", 0, 0, 500_000_000),
            ("PT1M30S", 0, 90, 0),
        ] {
            let d: Duration = text.parse().unwrap();
            assert_eq!(
                (d.days(), d.seconds(), d.nanoseconds()),
                (days, seconds, nanos),
                "{text}"
            );
            assert_eq!(d.to_string(), text, "roundtrip {text}");
        }
    }

    #[test]
    fn parse_folds_weeks_to_days_in_display() {
        let d: Duration = "P2W".parse().unwrap();
        assert_eq!(d.days(), 14);
        assert_eq!(d.to_string(), "P14D");
    }

    #[test]
    fn parse_rejects_malformed_durations() {
        for bad in [
            "", "P", "PT", "1H", "P1H", "PT5S1H", "PT1H1H", "P-1D", "PTS",
        ] {
            assert!(bad.parse::<Duration>().is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn signed_duration_before_and_after() {
        let fifteen_min: Duration = "PT15M".parse().unwrap();
        let before = SignedDuration::before(fifteen_min);
        assert!(before.is_before());
        assert_eq!(before.to_string(), "-PT15M");

        let after: SignedDuration = "PT15M".parse().unwrap();
        assert!(!after.is_before());
        assert_eq!(after.to_string(), "PT15M");

        // Leading `+` parses as positive.
        assert_eq!("+PT15M".parse::<SignedDuration>().unwrap(), after);
    }

    #[test]
    fn signed_zero_has_single_representation() {
        let zero = SignedDuration::before(Duration::ZERO);
        assert!(!zero.is_before());
        assert_eq!(zero.to_string(), "PT0S");
        assert_eq!(
            SignedDuration::before(Duration::ZERO),
            SignedDuration::after(Duration::ZERO)
        );
    }

    #[test]
    fn serde_roundtrips_as_strings() {
        let d: Duration = "P1DT2H".parse().unwrap();
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"P1DT2H\"");
        assert_eq!(serde_json::from_str::<Duration>("\"P1DT2H\"").unwrap(), d);

        let s = SignedDuration::before("PT10M".parse().unwrap());
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"-PT10M\"");
        assert_eq!(
            serde_json::from_str::<SignedDuration>("\"-PT10M\"").unwrap(),
            s
        );
    }
}
