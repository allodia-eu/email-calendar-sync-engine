//! Recurrence rules.

use core::num::{NonZeroI32, NonZeroU32};

use serde::{Deserialize, Serialize};

use crate::time::LocalDateTime;

/// How often a rule recurs (JSCalendar `frequency`, RFC 8984 §4.3.3; iCalendar
/// `FREQ`). A closed set — an unknown frequency is invalid, not preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Frequency {
    /// Once per year.
    Yearly,
    /// Once per month.
    Monthly,
    /// Once per week.
    Weekly,
    /// Once per day.
    Daily,
    /// Once per hour.
    Hourly,
    /// Once per minute.
    Minutely,
    /// Once per second.
    Secondly,
}

/// A day of the week (JSCalendar weekday tokens; iCalendar `BYDAY`/`WKST`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    /// Monday.
    Mo,
    /// Tuesday.
    Tu,
    /// Wednesday.
    We,
    /// Thursday.
    Th,
    /// Friday.
    Fr,
    /// Saturday.
    Sa,
    /// Sunday.
    Su,
}

/// How a non-Gregorian rule handles an invalid date (JSCalendar `skip`,
/// RFC 8984 §4.3.3; RFC 7529 `SKIP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecurrenceSkip {
    /// Skip invalid dates (the default).
    Omit,
    /// Move to the previous valid date.
    Backward,
    /// Move to the next valid date.
    Forward,
}

/// A weekday optionally restricted to its nth occurrence within the period
/// (JSCalendar `NDay`, the elements of `byDay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NDay {
    /// The weekday.
    pub day: Weekday,
    /// The 1-based occurrence within the period; negative counts from the end
    /// (e.g. `-1` is the last). Must be non-zero. `None` means every occurrence.
    pub nth_of_period: Option<NonZeroI32>,
}

/// The termination of a recurrence rule.
///
/// JSCalendar's `count` and `until` are mutually exclusive (RFC 8984 §4.3.3);
/// this enum makes setting both unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RecurrenceBound {
    /// Recurs forever.
    Unbounded,
    /// Recurs a fixed number of times (including the first instance).
    Count(NonZeroU32),
    /// Recurs until the given wall-clock time, interpreted in the event's zone
    /// (inclusive).
    Until(LocalDateTime),
}

/// A recurrence rule (JSCalendar `RecurrenceRule`, RFC 8984 §4.3.3).
///
/// Stored structurally, never as an `RRULE` string. Empty `by_*` vectors mean
/// the part is absent. Component ranges (`by_hour` 0–23, `by_second` 0–60, …)
/// follow the spec and are validated at the adapter/expansion boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceRule {
    /// The base frequency.
    pub frequency: Frequency,
    /// The interval between recurrences (default 1, never 0).
    pub interval: NonZeroU32,
    /// The CLDR calendar system, if non-Gregorian (RFC 7529 `RSCALE`); `None`
    /// means Gregorian.
    pub rscale: Option<String>,
    /// How to handle invalid dates under a non-Gregorian `rscale`.
    pub skip: RecurrenceSkip,
    /// The day a week starts on, affecting weekly/`byWeekNo` rules.
    pub first_day_of_week: Weekday,
    /// `BYDAY`: which weekdays (optionally nth) recur.
    pub by_day: Vec<NDay>,
    /// `BYMONTHDAY`: days of the month (negatives count from the end).
    pub by_month_day: Vec<i32>,
    /// `BYMONTH`: months as 1-based strings, with an optional `L` leap suffix.
    pub by_month: Vec<String>,
    /// `BYYEARDAY`: days of the year (negatives count from the end).
    pub by_year_day: Vec<i32>,
    /// `BYWEEKNO`: ISO week numbers (negatives count from the end).
    pub by_week_no: Vec<i32>,
    /// `BYHOUR`: hours, 0–23.
    pub by_hour: Vec<u8>,
    /// `BYMINUTE`: minutes, 0–59.
    pub by_minute: Vec<u8>,
    /// `BYSECOND`: seconds, 0–60 (60 for a leap second).
    pub by_second: Vec<u8>,
    /// `BYSETPOS`: which positions within the expansion are kept.
    pub by_set_position: Vec<i32>,
    /// How the rule terminates.
    pub bound: RecurrenceBound,
}

impl RecurrenceRule {
    /// Creates a rule with the given frequency, interval 1, Gregorian calendar,
    /// no `by_*` parts, and no termination.
    #[must_use]
    pub fn new(frequency: Frequency) -> Self {
        Self {
            frequency,
            interval: NonZeroU32::MIN,
            rscale: None,
            skip: RecurrenceSkip::Omit,
            first_day_of_week: Weekday::Mo,
            by_day: Vec::new(),
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            by_year_day: Vec::new(),
            by_week_no: Vec::new(),
            by_hour: Vec::new(),
            by_minute: Vec::new(),
            by_second: Vec::new(),
            by_set_position: Vec::new(),
            bound: RecurrenceBound::Unbounded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rule_defaults() {
        let rule = RecurrenceRule::new(Frequency::Weekly);
        assert_eq!(rule.interval.get(), 1);
        assert_eq!(rule.first_day_of_week, Weekday::Mo);
        assert_eq!(rule.bound, RecurrenceBound::Unbounded);
        assert!(rule.by_day.is_empty());
    }

    #[test]
    fn count_and_until_are_mutually_exclusive_by_construction() {
        // There is no way to represent both; a rule has exactly one `bound`.
        let mut rule = RecurrenceRule::new(Frequency::Daily);
        rule.bound = RecurrenceBound::Count(NonZeroU32::new(10).unwrap());
        assert!(matches!(rule.bound, RecurrenceBound::Count(_)));
        rule.bound = RecurrenceBound::Until("2025-01-01T00:00:00".parse().unwrap());
        assert!(matches!(rule.bound, RecurrenceBound::Until(_)));
    }

    #[test]
    fn nth_weekday_rule_roundtrips() {
        let mut rule = RecurrenceRule::new(Frequency::Monthly);
        rule.by_day = vec![NDay {
            day: Weekday::Th,
            nth_of_period: Some(NonZeroI32::new(-1).unwrap()), // last Thursday
        }];
        let json = serde_json::to_string(&rule).unwrap();
        assert_eq!(serde_json::from_str::<RecurrenceRule>(&json).unwrap(), rule);
    }

    #[test]
    fn zero_interval_is_rejected_on_deserialize() {
        let json = r#"{"frequency":"daily","interval":0,"rscale":null,"skip":"omit",
            "first_day_of_week":"mo","by_day":[],"by_month_day":[],"by_month":[],
            "by_year_day":[],"by_week_no":[],"by_hour":[],"by_minute":[],"by_second":[],
            "by_set_position":[],"bound":"Unbounded"}"#;
        assert!(serde_json::from_str::<RecurrenceRule>(json).is_err());
    }

    #[test]
    fn frequency_and_weekday_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&Frequency::Yearly).unwrap(),
            "\"yearly\""
        );
        assert_eq!(serde_json::to_string(&Weekday::Su).unwrap(), "\"su\"");
        // An unknown frequency is an error, not preserved.
        assert!(serde_json::from_str::<Frequency>("\"fortnightly\"").is_err());
    }
}
