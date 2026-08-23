//! Tests for the JSCalendar `RecurrenceRule` renderer.

use core::num::{NonZeroI32, NonZeroU32};

use super::*;

fn rule(frequency: Frequency) -> RecurrenceRule {
    RecurrenceRule::new(frequency)
}

fn nth(day: Weekday, n: Option<i32>) -> NDay {
    NDay {
        day,
        nth_of_period: n.and_then(NonZeroI32::new),
    }
}

#[test]
fn a_bare_rule_is_typed_and_carries_only_its_frequency() {
    // The defaults JSCalendar already assumes (interval 1, firstDayOfWeek mo) are omitted,
    // so the object says only what the rule actually states.
    let out = render_rule(&rule(Frequency::Weekly)).unwrap();
    assert_eq!(
        out,
        json!({ "@type": "RecurrenceRule", "frequency": "weekly" })
    );
}

#[test]
fn the_ux_presets_render_as_jscalendar() {
    let mut weekly = rule(Frequency::Weekly);
    weekly.by_day = vec![nth(Weekday::Mo, None)];
    assert_eq!(
        render_rule(&weekly).unwrap()["byDay"],
        json!([{ "@type": "NDay", "day": "mo" }])
    );

    let mut fourth_monday = rule(Frequency::Monthly);
    fourth_monday.by_day = vec![nth(Weekday::Mo, Some(4))];
    assert_eq!(
        render_rule(&fourth_monday).unwrap()["byDay"],
        json!([{ "@type": "NDay", "day": "mo", "nthOfPeriod": 4 }])
    );

    let mut last_friday = rule(Frequency::Monthly);
    last_friday.by_day = vec![nth(Weekday::Fr, Some(-1))];
    assert_eq!(
        render_rule(&last_friday).unwrap()["byDay"],
        json!([{ "@type": "NDay", "day": "fr", "nthOfPeriod": -1 }])
    );
}

#[test]
fn interval_and_week_start_appear_only_when_they_differ_from_the_default() {
    let mut r = rule(Frequency::Weekly);
    assert!(render_rule(&r).unwrap().get("interval").is_none());
    assert!(render_rule(&r).unwrap().get("firstDayOfWeek").is_none());

    r.interval = NonZeroU32::new(2).unwrap();
    r.first_day_of_week = Weekday::Su;
    let out = render_rule(&r).unwrap();
    assert_eq!(out["interval"], 2);
    assert_eq!(out["firstDayOfWeek"], "su");
}

#[test]
fn a_bound_becomes_count_or_until_but_never_both() {
    let mut counted = rule(Frequency::Daily);
    counted.bound = RecurrenceBound::Count(NonZeroU32::new(5).unwrap());
    let out = render_rule(&counted).unwrap();
    assert_eq!(out["count"], 5);
    assert!(out.get("until").is_none());

    let mut until = rule(Frequency::Daily);
    until.bound = RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    let out = render_rule(&until).unwrap();
    assert_eq!(out["until"], "2026-10-26T23:59:59");
    assert!(out.get("count").is_none());

    let unbounded = render_rule(&rule(Frequency::Daily)).unwrap();
    assert!(unbounded.get("count").is_none() && unbounded.get("until").is_none());
}

#[test]
fn until_stays_local_because_jscalendar_reads_it_in_the_events_zone() {
    // The opposite of iCalendar, which requires UTC (RFC 5545 §3.3.10). Converting here
    // would move the end of the series by the zone's offset.
    let mut r = rule(Frequency::Weekly);
    r.bound = RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    assert_eq!(render_rule(&r).unwrap()["until"], "2026-10-26T23:59:59");
}

#[test]
fn every_by_part_renders_under_its_jscalendar_name() {
    let mut r = rule(Frequency::Yearly);
    r.by_month_day = vec![1, -1];
    r.by_month = vec!["3".to_owned()];
    r.by_year_day = vec![100];
    r.by_week_no = vec![-2];
    r.by_hour = vec![9];
    r.by_minute = vec![30];
    r.by_second = vec![0];
    r.by_set_position = vec![-1];

    let out = render_rule(&r).unwrap();
    assert_eq!(out["byMonthDay"], json!([1, -1]));
    assert_eq!(out["byMonth"], json!(["3"]));
    assert_eq!(out["byYearDay"], json!([100]));
    assert_eq!(out["byWeekNo"], json!([-2]));
    assert_eq!(out["byHour"], json!([9]));
    assert_eq!(out["byMinute"], json!([30]));
    assert_eq!(out["bySecond"], json!([0]));
    assert_eq!(out["bySetPosition"], json!([-1]));
}

#[test]
fn an_empty_by_part_is_absent_rather_than_an_empty_array() {
    let out = render_rule(&rule(Frequency::Daily)).unwrap();
    for key in [
        "byDay",
        "byMonthDay",
        "byMonth",
        "byYearDay",
        "byWeekNo",
        "byHour",
        "byMinute",
        "bySecond",
        "bySetPosition",
    ] {
        assert!(out.get(key).is_none(), "{key} should be absent");
    }
}

#[test]
fn a_non_gregorian_rule_is_refused() {
    // JSCalendar could carry `rscale`, but this engine never expands such a rule — so
    // writing one would store a series that then draws as empty.
    let mut r = rule(Frequency::Yearly);
    r.rscale = Some("HEBREW".to_owned());
    let err = render_rule(&r).unwrap_err().to_string();
    assert!(err.contains("RSCALE=HEBREW"), "{err}");
}

#[test]
fn every_frequency_and_weekday_has_a_token() {
    for f in [
        Frequency::Secondly,
        Frequency::Minutely,
        Frequency::Hourly,
        Frequency::Daily,
        Frequency::Weekly,
        Frequency::Monthly,
        Frequency::Yearly,
    ] {
        assert!(render_rule(&rule(f)).unwrap()["frequency"].is_string());
    }
    for d in [
        Weekday::Mo,
        Weekday::Tu,
        Weekday::We,
        Weekday::Th,
        Weekday::Fr,
        Weekday::Sa,
        Weekday::Su,
    ] {
        let mut r = rule(Frequency::Weekly);
        r.by_day = vec![nth(d, None)];
        assert!(render_rule(&r).unwrap()["byDay"][0]["day"].is_string());
    }
}
