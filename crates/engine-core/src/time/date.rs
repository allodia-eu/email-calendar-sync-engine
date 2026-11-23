//! Zoneless calendar dates.

use core::{fmt, str::FromStr};

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

    /// The date `months` whole calendar months before this one, the day clamped to the length of
    /// the month it lands in: 31 March less one month is 28 February, or the 29th in a leap year.
    ///
    /// This is the one rule for "N months back" that every host shares, so a mail window built
    /// from it means the same day wherever it is built. `None` when the result falls below the
    /// representable range.
    #[must_use]
    pub fn checked_months_before(self, months: u32) -> Option<Self> {
        let index = i64::from(self.year()) * 12 + i64::from(self.month()) - 1 - i64::from(months);
        let year = i32::try_from(index.div_euclid(12)).ok()?;
        let month = Month::try_from(u8::try_from(index.rem_euclid(12) + 1).ok()?).ok()?;
        let day = self.day().min(month.length(year));
        Date::from_calendar_date(year, month, day).ok().map(Self)
    }

    /// The date `days` days later, or earlier for a negative `days`. `None` outside the
    /// representable range.
    #[must_use]
    pub fn checked_add_days(self, days: i64) -> Option<Self> {
        let day = i64::from(self.0.to_julian_day()).checked_add(days)?;
        Date::from_julian_day(i32::try_from(day).ok()?)
            .ok()
            .map(Self)
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

    fn date(raw: &str) -> CalendarDate {
        raw.parse().unwrap()
    }

    #[test]
    fn months_back_clamp_the_day_to_the_month_they_land_in() {
        for (from, months, expected) in [
            ("2026-03-31", 1, "2026-02-28"),
            ("2024-03-31", 1, "2024-02-29"),
            ("2024-02-29", 12, "2023-02-28"),
            ("2024-02-29", 48, "2020-02-29"),
            ("2026-05-31", 1, "2026-04-30"),
            ("2026-01-31", 1, "2025-12-31"),
            ("2026-01-15", 13, "2024-12-15"),
            ("2026-05-31", 3, "2026-02-28"),
            ("2026-08-31", 24, "2024-08-31"),
            ("2026-08-15", 1200, "1926-08-15"),
            ("2026-08-15", 0, "2026-08-15"),
        ] {
            assert_eq!(
                date(from).checked_months_before(months),
                Some(date(expected)),
                "{from} less {months} months"
            );
        }
    }

    #[test]
    fn months_back_below_the_representable_range_are_none() {
        let earliest = CalendarDate(Date::MIN);
        assert_eq!(earliest.checked_months_before(0), Some(earliest));
        assert_eq!(earliest.checked_months_before(1), None);
        assert_eq!(date("2026-08-15").checked_months_before(u32::MAX), None);
    }

    #[test]
    fn days_move_either_way_and_stop_at_the_range_edge() {
        assert_eq!(
            date("2026-02-27").checked_add_days(2),
            Some(date("2026-03-01"))
        );
        assert_eq!(
            date("2024-03-01").checked_add_days(-1),
            Some(date("2024-02-29"))
        );
        assert_eq!(
            date("2026-01-01").checked_add_days(-120),
            Some(date("2025-09-03"))
        );
        assert_eq!(CalendarDate(Date::MAX).checked_add_days(1), None);
        assert_eq!(CalendarDate(Date::MIN).checked_add_days(-1), None);
        assert_eq!(date("2026-01-01").checked_add_days(i64::MAX), None);
    }

    #[test]
    fn dates_order_chronologically() {
        let earlier: CalendarDate = "2021-01-01".parse().unwrap();
        let later: CalendarDate = "2021-12-31".parse().unwrap();
        assert!(earlier < later);
    }
}
