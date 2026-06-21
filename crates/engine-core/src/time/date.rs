//! Zoneless calendar dates.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};
use time::{Date, Month};

use super::{TimeError, format_date, parse_ymd};

/// A zoneless calendar date (RFC 5545 `DATE`; JSCalendar all-day values).
///
/// A calendar date has no time, no zone, and no DST: it is the same date
/// everywhere. All-day and multi-day events use this; a zone must never be
/// attached to it (`calendar-semantics.md`). The canonical form is `YYYY-MM-DD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CalendarDate(Date);

impl CalendarDate {
    /// Creates a calendar date from its components.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::OutOfRange`] if the components do not form a real
    /// date (for example, 29 February in a non-leap year).
    pub fn new(year: i32, month: u8, day: u8) -> Result<Self, TimeError> {
        let month = Month::try_from(month).map_err(|_| TimeError::OutOfRange)?;
        Date::from_calendar_date(year, month, day)
            .map(Self)
            .map_err(|_| TimeError::OutOfRange)
    }

    /// The underlying `time` value, for date arithmetic within the time module
    /// (e.g. an all-day `DTSTART`/`DTEND` span in whole days).
    pub(crate) fn as_date(self) -> Date {
        self.0
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
}

impl fmt::Display for CalendarDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_date(self.0))
    }
}

impl FromStr for CalendarDate {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_ymd(s).map(Self)
    }
}

impl TryFrom<String> for CalendarDate {
    type Error = TimeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<CalendarDate> for String {
    fn from(value: CalendarDate) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_validated() {
        let date = CalendarDate::new(2024, 2, 29).unwrap();
        assert_eq!((date.year(), date.month(), date.day()), (2024, 2, 29));
        assert_eq!(CalendarDate::new(2023, 2, 29), Err(TimeError::OutOfRange));
        assert_eq!(CalendarDate::new(2023, 13, 1), Err(TimeError::OutOfRange));
    }

    #[test]
    fn roundtrips_through_canonical_string() {
        let date: CalendarDate = "2021-07-04".parse().unwrap();
        assert_eq!(date.to_string(), "2021-07-04");
        let json = serde_json::to_string(&date).unwrap();
        assert_eq!(json, "\"2021-07-04\"");
        assert_eq!(serde_json::from_str::<CalendarDate>(&json).unwrap(), date);
    }

    #[test]
    fn dates_order_chronologically() {
        let earlier: CalendarDate = "2021-01-01".parse().unwrap();
        let later: CalendarDate = "2021-12-31".parse().unwrap();
        assert!(earlier < later);
    }
}
