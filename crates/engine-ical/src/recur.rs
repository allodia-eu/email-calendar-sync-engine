//! Parsing an iCalendar `RRULE` into the structural [`RecurrenceRule`]
//! (RFC 5545 §3.3.10).
//!
//! The RFC 5545 RRULE-string grammar lives in one shared place —
//! [`engine_core::calendar::parse_rrule`] — so this is a thin wrapper that maps a parse
//! failure onto [`IcalError`]. Google Calendar's adapter parses the same wire
//! shape through the same shared parser.

use engine_core::calendar::{RecurrenceRule, parse_rrule as parse_rrule_shared};

use crate::error::IcalError;

/// Parses an `RRULE` value into a [`RecurrenceRule`].
///
/// # Errors
///
/// Returns [`IcalError`] if `FREQ` is missing/unknown, `COUNT` is not a positive
/// integer, or `UNTIL` is malformed.
pub(crate) fn parse_rrule(value: &str) -> Result<RecurrenceRule, IcalError> {
    parse_rrule_shared(value).map_err(|e| IcalError::new(e.to_string()))
}

#[cfg(test)]
mod tests {
    use core::num::{NonZeroI32, NonZeroU32};

    use engine_core::calendar::{Frequency, NDay, RecurrenceBound, Weekday};

    use super::*;

    #[test]
    fn parses_the_seed_weekly_rule() {
        // The recurring-weekly fixture: FREQ=WEEKLY;COUNT=8;BYDAY=MO.
        let rule = parse_rrule("FREQ=WEEKLY;COUNT=8;BYDAY=MO").unwrap();
        assert_eq!(rule.frequency, Frequency::Weekly);
        assert_eq!(rule.interval, NonZeroU32::new(1).unwrap());
        assert_eq!(
            rule.bound,
            RecurrenceBound::Count(NonZeroU32::new(8).unwrap())
        );
        assert_eq!(
            rule.by_day,
            vec![NDay {
                day: Weekday::Mo,
                nth_of_period: None
            }]
        );
    }

    #[test]
    fn parses_nth_weekday_and_bymonth() {
        // A VTIMEZONE-style rule: last Sunday of March.
        let rule = parse_rrule("FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU").unwrap();
        assert_eq!(rule.frequency, Frequency::Yearly);
        assert_eq!(rule.by_month, vec!["3".to_owned()]);
        assert_eq!(
            rule.by_day,
            vec![NDay {
                day: Weekday::Su,
                nth_of_period: Some(NonZeroI32::new(-1).unwrap())
            }]
        );
    }

    #[test]
    fn interval_until_and_negatives() {
        let rule =
            parse_rrule("FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=-1;UNTIL=20261231T000000Z").unwrap();
        assert_eq!(rule.interval, NonZeroU32::new(2).unwrap());
        assert_eq!(rule.by_month_day, vec![-1]);
        assert!(matches!(rule.bound, RecurrenceBound::Until(_)));
    }

    #[test]
    fn wkst_and_staged_parts_are_preserved() {
        let rule = parse_rrule("FREQ=WEEKLY;WKST=SU;BYSETPOS=1,-1;BYHOUR=9").unwrap();
        assert_eq!(rule.first_day_of_week, Weekday::Su);
        assert_eq!(rule.by_set_position, vec![1, -1]);
        assert_eq!(rule.by_hour, vec![9]);
    }

    #[test]
    fn missing_or_unknown_frequency_is_rejected() {
        assert!(parse_rrule("COUNT=3").is_err());
        assert!(parse_rrule("FREQ=FORTNIGHTLY").is_err());
    }

    #[test]
    fn count_zero_is_rejected_not_treated_as_unbounded() {
        // COUNT=0 must NOT silently fall through to an unbounded series (which
        // would expand to the whole horizon); RFC 5545 requires a positive COUNT.
        assert!(parse_rrule("FREQ=DAILY;COUNT=0").is_err());
        // A non-numeric COUNT is likewise rejected, not ignored.
        assert!(parse_rrule("FREQ=DAILY;COUNT=lots").is_err());
    }
}
