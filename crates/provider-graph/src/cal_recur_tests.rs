//! Tests for `patternedRecurrence` → `Recurrence` across every pattern/range shape.

use engine_core::calendar::{Frequency, RecurrenceBound, Weekday};
use serde_json::json;

use super::*;

/// Wraps a `pattern`+`range` in an event's `recurrence` and returns the single rule.
fn rule(pattern: serde_json::Value, range: serde_json::Value) -> RecurrenceRule {
    let mut recur = serde_json::Map::new();
    recur.insert("pattern".to_owned(), pattern);
    recur.insert("range".to_owned(), range);
    let value = json!({ "recurrence": recur });
    let recurrence = parse_recurrence(&value).unwrap().expect("a rule");
    assert_eq!(recurrence.rules.len(), 1);
    recurrence.rules.into_iter().next().unwrap()
}

fn no_end() -> serde_json::Value {
    json!({ "type": "noEnd" })
}

#[test]
fn no_recurrence_key_is_none() {
    assert!(parse_recurrence(&json!({})).unwrap().is_none());
    assert!(
        parse_recurrence(&json!({ "recurrence": null }))
            .unwrap()
            .is_none()
    );
}

#[test]
fn daily_maps_frequency_and_interval() {
    let rule = rule(json!({ "type": "daily", "interval": 3 }), no_end());
    assert_eq!(rule.frequency, Frequency::Daily);
    assert_eq!(rule.interval.get(), 3);
    assert!(matches!(rule.bound, RecurrenceBound::Unbounded));
}

#[test]
fn weekly_maps_days_and_first_day_of_week() {
    let rule = rule(
        json!({ "type": "weekly", "interval": 1, "daysOfWeek": ["monday", "thursday"],
                "firstDayOfWeek": "sunday" }),
        no_end(),
    );
    assert_eq!(rule.frequency, Frequency::Weekly);
    assert_eq!(rule.first_day_of_week, Weekday::Su);
    let days: Vec<Weekday> = rule.by_day.iter().map(|d| d.day).collect();
    assert_eq!(days, vec![Weekday::Mo, Weekday::Th]);
    assert!(rule.by_day.iter().all(|d| d.nth_of_period.is_none()));
}

#[test]
fn absolute_monthly_maps_day_of_month() {
    let rule = rule(
        json!({ "type": "absoluteMonthly", "dayOfMonth": 15 }),
        no_end(),
    );
    assert_eq!(rule.frequency, Frequency::Monthly);
    assert_eq!(rule.by_month_day, vec![15]);
}

#[test]
fn relative_monthly_maps_nth_weekday() {
    // "last Friday" — index `last` is nth -1 on Friday.
    let rule = rule(
        json!({ "type": "relativeMonthly", "daysOfWeek": ["friday"], "index": "last" }),
        no_end(),
    );
    assert_eq!(rule.frequency, Frequency::Monthly);
    assert_eq!(rule.by_day.len(), 1);
    assert_eq!(rule.by_day[0].day, Weekday::Fr);
    assert_eq!(rule.by_day[0].nth_of_period.unwrap().get(), -1);
}

#[test]
fn absolute_yearly_maps_month_and_day() {
    let rule = rule(
        json!({ "type": "absoluteYearly", "month": 12, "dayOfMonth": 25 }),
        no_end(),
    );
    assert_eq!(rule.frequency, Frequency::Yearly);
    assert_eq!(rule.by_month, vec!["12".to_owned()]);
    assert_eq!(rule.by_month_day, vec![25]);
}

#[test]
fn relative_yearly_maps_month_and_nth_weekday() {
    // "second Tuesday of March".
    let rule = rule(
        json!({ "type": "relativeYearly", "month": 3, "daysOfWeek": ["tuesday"], "index": "second" }),
        no_end(),
    );
    assert_eq!(rule.frequency, Frequency::Yearly);
    assert_eq!(rule.by_month, vec!["3".to_owned()]);
    assert_eq!(rule.by_day[0].nth_of_period.unwrap().get(), 2);
}

#[test]
fn numbered_range_is_a_count() {
    let rule = rule(
        json!({ "type": "daily" }),
        json!({ "type": "numbered", "numberOfOccurrences": 10 }),
    );
    assert!(matches!(rule.bound, RecurrenceBound::Count(n) if n.get() == 10));
}

#[test]
fn end_date_range_is_an_until_at_end_of_day() {
    let rule = rule(
        json!({ "type": "weekly", "daysOfWeek": ["monday"] }),
        json!({ "type": "endDate", "startDate": "2026-08-03", "endDate": "2026-10-05" }),
    );
    let RecurrenceBound::Until(until) = rule.bound else {
        panic!("expected an UNTIL, got {:?}", rule.bound);
    };
    // Bounded at the end of the last day so an occurrence on it is included.
    assert_eq!(until.to_string(), "2026-10-05T23:59:59");
}

#[test]
fn weekly_without_days_and_an_unknown_index_degrade_gracefully() {
    // A weekly pattern with no `daysOfWeek` yields no BYDAY (defensive; Graph always sends it).
    let weekly = rule(json!({ "type": "weekly" }), no_end());
    assert!(weekly.by_day.is_empty());
    // An `index` Graph would never send leaves the nth unset rather than erroring.
    let relative = rule(
        json!({ "type": "relativeMonthly", "daysOfWeek": ["monday"], "index": "fifth" }),
        no_end(),
    );
    assert!(relative.by_day[0].nth_of_period.is_none());
    // An out-of-range month is dropped.
    let yearly = rule(
        json!({ "type": "absoluteYearly", "month": 13, "dayOfMonth": 1 }),
        no_end(),
    );
    assert!(yearly.by_month.is_empty());
}

#[test]
fn malformed_recurrence_is_a_protocol_error() {
    // Unknown pattern type.
    assert!(
        parse_recurrence(&json!({
            "recurrence": { "pattern": { "type": "hourly" }, "range": { "type": "noEnd" } }
        }))
        .is_err()
    );
    // Numbered range with no count.
    assert!(
        parse_recurrence(&json!({
            "recurrence": { "pattern": { "type": "daily" }, "range": { "type": "numbered" } }
        }))
        .is_err()
    );
    // Unknown range type.
    assert!(
        parse_recurrence(&json!({
            "recurrence": { "pattern": { "type": "daily" }, "range": { "type": "forever" } }
        }))
        .is_err()
    );
    // A weekday name Graph would never send.
    assert!(
        parse_recurrence(&json!({
            "recurrence": {
                "pattern": { "type": "weekly", "daysOfWeek": ["someday"] },
                "range": { "type": "noEnd" }
            }
        }))
        .is_err()
    );
    // Missing pattern / range.
    assert!(parse_recurrence(&json!({ "recurrence": { "range": no_end() } })).is_err());
    assert!(
        parse_recurrence(&json!({ "recurrence": { "pattern": { "type": "daily" } } })).is_err()
    );
    // A malformed endDate.
    assert!(
        parse_recurrence(&json!({
            "recurrence": {
                "pattern": { "type": "daily" },
                "range": { "type": "endDate", "endDate": "not-a-date" }
            }
        }))
        .is_err()
    );
}
