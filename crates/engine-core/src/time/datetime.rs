//! Wall-clock and UTC date-times.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};
use time::{Date, Month, PrimitiveDateTime, Time};

use super::{TimeError, format_wall_clock, parse_wall_clock};

/// Builds a [`PrimitiveDateTime`] from individual components, validating each.
fn from_components(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> Result<PrimitiveDateTime, TimeError> {
    let month = Month::try_from(month).map_err(|_| TimeError::OutOfRange)?;
    let date = Date::from_calendar_date(year, month, day).map_err(|_| TimeError::OutOfRange)?;
    let time = Time::from_hms(hour, minute, second).map_err(|_| TimeError::OutOfRange)?;
    Ok(PrimitiveDateTime::new(date, time))
}

/// A wall-clock date-time with **no** zone or offset (JSCalendar `LocalDateTime`,
/// RFC 8984 §1.4.5; the local part of an iCalendar `DATE-TIME`).
///
/// This is the spine time type. The zone to associate with it comes from a
/// separate `timeZone` property; with no zone it is *floating* — the same
/// wall-clock time in every zone, not a fixed instant. Resolving it to an
/// instant (which needs tzdata) is done at query/display time elsewhere. The
/// canonical form is `YYYY-MM-DDThh:mm:ss`, with optional non-zero fractional
/// seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LocalDateTime(PrimitiveDateTime);

impl LocalDateTime {
    /// Creates a wall-clock date-time from its components (whole seconds).
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::OutOfRange`] if the components do not form a real
    /// date-time.
    pub fn new(
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<Self, TimeError> {
        from_components(year, month, day, hour, minute, second).map(Self)
    }

    /// Returns the year.
    #[must_use]
    pub fn year(self) -> i32 {
        self.0.year()
    }

    /// Returns the month, 1–12.
    #[must_use]
    pub fn month(self) -> u8 {
        u8::from(self.0.month())
    }

    /// Returns the day of the month, 1–31.
    #[must_use]
    pub fn day(self) -> u8 {
        self.0.day()
    }

    /// Returns the hour, 0–23.
    #[must_use]
    pub fn hour(self) -> u8 {
        self.0.hour()
    }

    /// Returns the minute, 0–59.
    #[must_use]
    pub fn minute(self) -> u8 {
        self.0.minute()
    }

    /// Returns the second, 0–59.
    #[must_use]
    pub fn second(self) -> u8 {
        self.0.second()
    }

    /// Returns the sub-second component in nanoseconds, 0..1_000_000_000.
    #[must_use]
    pub fn nanosecond(self) -> u32 {
        self.0.nanosecond()
    }
}

impl fmt::Display for LocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_wall_clock(self.0))
    }
}

impl FromStr for LocalDateTime {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_wall_clock(s).map(Self)
    }
}

impl TryFrom<String> for LocalDateTime {
    type Error = TimeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<LocalDateTime> for String {
    fn from(value: LocalDateTime) -> Self {
        value.to_string()
    }
}

/// A true UTC instant (JSCalendar `UTCDateTime`, RFC 8984 §1.4.4).
///
/// Used only for **metadata** timestamps — `created`, `updated`, `DTSTAMP`,
/// an absolute alert trigger, an acknowledgement — never for an event's
/// scheduled start (which is wall-clock; see [`LocalDateTime`] and
/// [`super::CalendarDateTime`]). The canonical form is `YYYY-MM-DDThh:mm:ssZ`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UtcDateTime(PrimitiveDateTime);

impl UtcDateTime {
    /// Creates a UTC instant from its components (whole seconds).
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::OutOfRange`] if the components do not form a real
    /// date-time.
    pub fn new(
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<Self, TimeError> {
        from_components(year, month, day, hour, minute, second).map(Self)
    }

    /// Returns the year.
    #[must_use]
    pub fn year(self) -> i32 {
        self.0.year()
    }

    /// Returns the month, 1–12.
    #[must_use]
    pub fn month(self) -> u8 {
        u8::from(self.0.month())
    }

    /// Returns the day of the month, 1–31.
    #[must_use]
    pub fn day(self) -> u8 {
        self.0.day()
    }

    /// Returns the hour, 0–23.
    #[must_use]
    pub fn hour(self) -> u8 {
        self.0.hour()
    }

    /// Returns the minute, 0–59.
    #[must_use]
    pub fn minute(self) -> u8 {
        self.0.minute()
    }

    /// Returns the second, 0–59.
    #[must_use]
    pub fn second(self) -> u8 {
        self.0.second()
    }

    /// Returns the sub-second component in nanoseconds, 0..1_000_000_000.
    #[must_use]
    pub fn nanosecond(self) -> u32 {
        self.0.nanosecond()
    }
}

impl fmt::Display for UtcDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}Z", format_wall_clock(self.0))
    }
}

impl FromStr for UtcDateTime {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let body = s.strip_suffix('Z').ok_or_else(|| TimeError::Malformed {
            expected: "UTC date-time ending in Z",
            found: s.to_owned(),
        })?;
        parse_wall_clock(body).map(Self)
    }
}

impl TryFrom<String> for UtcDateTime {
    type Error = TimeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<UtcDateTime> for String {
    fn from(value: UtcDateTime) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_date_time_roundtrips() {
        let dt = LocalDateTime::new(2006, 1, 2, 15, 4, 5).unwrap();
        assert_eq!(dt.to_string(), "2006-01-02T15:04:05");
        assert_eq!("2006-01-02T15:04:05".parse::<LocalDateTime>().unwrap(), dt);
        assert_eq!(dt.year(), 2006);
        assert_eq!(dt.hour(), 15);
        assert_eq!(dt.nanosecond(), 0);
    }

    #[test]
    fn local_date_time_keeps_fractional_seconds() {
        let dt: LocalDateTime = "2006-01-02T15:04:05.003".parse().unwrap();
        assert_eq!(dt.nanosecond(), 3_000_000);
        assert_eq!(dt.to_string(), "2006-01-02T15:04:05.003");
    }

    #[test]
    fn utc_date_time_requires_and_renders_z() {
        let dt: UtcDateTime = "2010-10-10T10:10:10Z".parse().unwrap();
        assert_eq!(dt.to_string(), "2010-10-10T10:10:10Z");
        // The same wall clock without `Z` is not a valid UTC instant.
        assert!("2010-10-10T10:10:10".parse::<UtcDateTime>().is_err());
    }

    #[test]
    fn utc_date_time_normalizes_zero_fraction() {
        // RFC 8984 §1.4.4: `.000` is invalid input; we normalize it away.
        let dt: UtcDateTime = "2010-10-10T10:10:10.000Z".parse().unwrap();
        assert_eq!(dt.to_string(), "2010-10-10T10:10:10Z");
    }

    #[test]
    fn invalid_components_rejected() {
        assert_eq!(
            LocalDateTime::new(2021, 2, 29, 0, 0, 0),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            LocalDateTime::new(2021, 1, 1, 24, 0, 0),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn instants_order_chronologically() {
        let a: UtcDateTime = "2021-01-01T00:00:00Z".parse().unwrap();
        let b: UtcDateTime = "2021-01-01T00:00:01Z".parse().unwrap();
        assert!(a < b);
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(j, "\"2021-01-01T00:00:00Z\"");
        assert_eq!(serde_json::from_str::<UtcDateTime>(&j).unwrap(), a);
    }
}
