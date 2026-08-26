// SPDX-License-Identifier: MPL-2.0

use super::{
    CAL_RECURRENCE_DAY_OF_MONTH, CAL_RECURRENCE_DAY_OF_WEEK, CAL_RECURRENCE_INTERVAL,
    CAL_RECURRENCE_MONTH_OF_YEAR, CAL_RECURRENCE_OCCURRENCES, CAL_RECURRENCE_TYPE,
    CAL_RECURRENCE_UNTIL, CAL_RECURRENCE_WEEK_OF_MONTH, PAGE_CALENDAR,
    fields::{parse_datetime_field, tag_label, text_value_opt},
    model::CalendarRecurrence,
};
use crate::wbxml::WbxmlElement;

// ============================================================================
// Container parse — Recurrence / Exceptions / Attendees (M8 Task 3)
// ============================================================================

/// Parse a `Recurrence` container ([MS-ASCAL] §2.2.2.37) into raw wire
/// values — RRULE conversion is downstream (M8 Task 6). Per-Type
/// requiredness (§2.2.2.37.1) is the converter's concern; here every child
/// is optional and permissive: non-numeric values warn + `None`,
/// out-of-spec-range values warn but are kept raw (downsync fidelity).
pub(super) fn parse_recurrence(elem: &WbxmlElement) -> CalendarRecurrence {
    let mut rec = CalendarRecurrence::default();
    let mut type_seen = false;
    for child in &elem.children {
        match (child.page, child.token) {
            // [MS-ASCAL] §2.2.2.45 (v20220429): enum {0,1,2,3,5,6}; unknown
            // values warn but are kept raw.
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE) => {
                type_seen = true;
                match text_value_opt(child) {
                    Some(raw) => match raw.parse::<u8>() {
                        Ok(n) => {
                            if !matches!(n, 0 | 1 | 2 | 3 | 5 | 6) {
                                log::warn!(
                                    "calendar Recurrence: Type {n} outside the [MS-ASCAL] \
                                     §2.2.2.45 enum {{0,1,2,3,5,6}}; keeping raw"
                                );
                            }
                            rec.recurrence_type = n;
                        }
                        Err(_) => {
                            log::warn!(
                                "calendar Recurrence: malformed Type \"{raw}\"; expected \
                                 a number, defaulting to 0"
                            );
                        }
                    },
                    None => {
                        log::warn!(
                            "calendar Recurrence: Type element without a text value; \
                             defaulting to 0"
                        );
                    }
                }
            }
            // §2.2.2.47: Compact DateTime string, kept verbatim.
            (PAGE_CALENDAR, CAL_RECURRENCE_UNTIL) => {
                rec.until = parse_datetime_field("Recurrence/Until", child);
            }
            // §2.2.2.32: unsignedShort, max 999.
            (PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES) => {
                rec.occurrences = parse_rec_number("Occurrences", child, |n| n <= 999, "max 999");
            }
            // §2.2.2.25: unsignedShort 0-999.
            (PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL) => {
                rec.interval = parse_rec_number("Interval", child, |n| n <= 999, "0-999");
            }
            // §2.2.2.15: bitmask 1..=127 (sums of 1/2/4/8/16/32/64 plus the
            // specials 62/65/127).
            (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK) => {
                rec.day_of_week =
                    parse_rec_number("DayOfWeek", child, |n| (1..=127).contains(&n), "1-127");
            }
            // §2.2.2.14: 1-31.
            (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH) => {
                rec.day_of_month =
                    parse_rec_number("DayOfMonth", child, |n| (1..=31).contains(&n), "1-31");
            }
            // §2.2.2.48: 1-5, 5 = last week of the month.
            (PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH) => {
                rec.week_of_month =
                    parse_rec_number("WeekOfMonth", child, |n| (1..=5).contains(&n), "1-5");
            }
            // §2.2.2.29: 1-12.
            (PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR) => {
                rec.month_of_year =
                    parse_rec_number("MonthOfYear", child, |n| (1..=12).contains(&n), "1-12");
            }
            _ => {
                // Covers CalendarType (0x37), IsLeapMonth (0x38),
                // FirstDayOfWeek (0x39) — not modeled by the v1 struct.
                log::debug!(
                    "calendar Recurrence: skipping unmodeled child {} (page {} token \
                     0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    if !type_seen {
        log::warn!(
            "calendar Recurrence: no Type child ([MS-ASCAL] §2.2.2.45 requires it in \
             protocol versions ≤ 14.1); defaulting to 0 (daily)"
        );
    }
    if rec.until.is_some() && rec.occurrences.is_some() {
        log::warn!(
            "calendar Recurrence: both Until and Occurrences present — they are \
             mutually exclusive per [MS-ASCAL] §2.2.2.47; keeping both raw"
        );
    }
    // Derived, not a wire token: "If neither value is set, the event has no
    // end date" ([MS-ASCAL] §2.2.2.37.1).
    rec.no_end = rec.until.is_none() && rec.occurrences.is_none();
    rec
}

/// Numeric `Recurrence` child: parse to u32. Non-numeric text warns +
/// `None`; parseable-but-out-of-spec-range warns and is kept raw (downsync
/// fidelity — the converter decides). `range` labels the spec range in the
/// warning.
fn parse_rec_number(
    name: &'static str,
    elem: &WbxmlElement,
    valid: impl Fn(u32) -> bool,
    range: &'static str,
) -> Option<u32> {
    let raw = text_value_opt(elem)?;
    match raw.parse::<u32>() {
        Ok(n) if valid(n) => Some(n),
        Ok(n) => {
            log::warn!(
                "calendar Recurrence: {name} value {n} outside the [MS-ASCAL] range \
                 ({range}); keeping raw"
            );
            Some(n)
        }
        Err(_) => {
            log::warn!(
                "calendar Recurrence: malformed {name} \"{raw}\"; expected a number, \
                 ignoring"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{CAL_RECURRENCE, PAGE_CALENDAR, parse_calendar_application_data},
        commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    };

    // ====================================================================
    // M8 Task 3 — Recurrence
    // ====================================================================

    /// Wrap recurrence children in an ApplicationData carrying a single
    /// `Recurrence` container.
    fn recurrence_app_data(children: Vec<WbxmlElement>) -> WbxmlElement {
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_RECURRENCE,
                children,
            )],
        )
    }

    /// Parse and unwrap the single recurrence of an ApplicationData.
    fn parse_recurrence(children: Vec<WbxmlElement>) -> CalendarRecurrence {
        let props =
            parse_calendar_application_data(&recurrence_app_data(children)).expect("parse ok");
        props
            .recurrence
            .expect("Recurrence container must parse to Some")
    }

    /// Type 0 (daily) — [MS-ASCAL] §4.4 "every other day" example; no
    /// Until/Occurrences ⇒ `no_end` is derived true (§2.2.2.37.1).
    #[test]
    fn parse_recurrence_type_0_daily_no_end() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "0"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "2"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 0,
                interval: Some(2),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Type 1 (weekly) — §4.4 "every weekday" example (DayOfWeek 62) with
    /// an Until ⇒ bounded series, `no_end` false.
    #[test]
    fn parse_recurrence_type_1_weekly_with_until() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "62"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "20261231T235959Z"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 1,
                interval: Some(1),
                day_of_week: Some(62),
                until: Some("20261231T235959Z".to_string()),
                no_end: false,
                ..Default::default()
            }
        );
    }

    /// Type 2 (monthly by day) — §4.4 "first day of every month" example,
    /// bounded by Occurrences instead of Until.
    #[test]
    fn parse_recurrence_type_2_monthly_with_occurrences() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "2"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "10"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 2,
                interval: Some(1),
                day_of_month: Some(1),
                occurrences: Some(10),
                no_end: false,
                ..Default::default()
            }
        );
    }

    /// Type 3 (monthly nth) — §4.4 "last day of every month" example:
    /// WeekOfMonth 5 (last) + DayOfWeek 127 (§2.2.2.15 special value).
    #[test]
    fn parse_recurrence_type_3_monthly_last_day() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "3"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH, "5"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "127"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 3,
                interval: Some(1),
                week_of_month: Some(5),
                day_of_week: Some(127),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Type 5 (yearly by date) — §4.4 "June 1 every year" example.
    #[test]
    fn parse_recurrence_type_5_yearly_by_date() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "5"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR, "6"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 5,
                interval: Some(1),
                day_of_month: Some(1),
                month_of_year: Some(6),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Type 6 (yearly nth weekday) — §4.4 "first Saturday of June" example.
    #[test]
    fn parse_recurrence_type_6_yearly_nth_weekday() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "6"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "64"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR, "6"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 6,
                interval: Some(1),
                week_of_month: Some(1),
                day_of_week: Some(64),
                month_of_year: Some(6),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Out-of-enum Type: the v20220429 enum is {0,1,2,3,5,6} (§2.2.2.45 —
    /// no value 4, no regenerate variants). Unknown values WARN but are
    /// kept raw (downsync fidelity; the converter decides). Non-numeric
    /// Type warns and defaults to 0.
    #[test]
    fn parse_recurrence_unknown_type_kept_raw() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "4"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
        ]);
        assert_eq!(rec.recurrence_type, 4, "out-of-enum Type kept raw");
        assert_eq!(rec.interval, Some(1));

        let rec = parse_recurrence(vec![WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_TYPE,
            "daily",
        )]);
        assert_eq!(rec.recurrence_type, 0, "non-numeric Type defaults to 0");
        assert!(rec.no_end);
    }

    /// Out-of-spec-range numeric children warn but are kept raw; both Until
    /// and Occurrences present (mutually exclusive per §2.2.2.47) warn and
    /// both are kept, with `no_end` false.
    #[test]
    fn parse_recurrence_out_of_range_children_kept_raw() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "0"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "5000"), // >999
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "255"), // >127
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH, "99"), // >31
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH, "9"), // >5
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR, "13"), // >12
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "1000"), // >999
        ]);
        assert_eq!(rec.interval, Some(5000));
        assert_eq!(rec.day_of_week, Some(255));
        assert_eq!(rec.day_of_month, Some(99));
        assert_eq!(rec.week_of_month, Some(9));
        assert_eq!(rec.month_of_year, Some(13));
        assert_eq!(rec.occurrences, Some(1000));

        // Malformed (non-numeric) children degrade to None.
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "weekly"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "not-a-date"),
        ]);
        assert_eq!(rec.interval, None);
        assert_eq!(rec.until, None);
        assert!(rec.no_end);

        // Until + Occurrences together: warn, keep both, series bounded.
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "0"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "20261231T235959Z"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "5"),
        ]);
        assert_eq!(rec.until.as_deref(), Some("20261231T235959Z"));
        assert_eq!(rec.occurrences, Some(5));
        assert!(!rec.no_end);
    }
}
