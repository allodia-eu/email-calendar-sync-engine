//! Tests for the `RecurrenceRule` → Graph `patternedRecurrence` renderer, including the
//! round-trip back through [`parse_recurrence`](crate::cal_recur::parse_recurrence).

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::calendar::Recurrence;

use super::*;
use crate::cal_recur::parse_recurrence;

fn start() -> CalendarDate {
    // A Monday, so a BYDAY-less weekly rule names "monday".
    CalendarDate::new(2026, 9, 7).unwrap()
}

fn rule(frequency: Frequency) -> RecurrenceRule {
    RecurrenceRule::new(frequency)
}

fn nday(day: Weekday, nth: Option<i32>) -> NDay {
    NDay {
        day,
        nth_of_period: nth.and_then(NonZeroI32::new),
    }
}

fn render(r: &RecurrenceRule) -> Value {
    render_recurrence(r, start()).expect("renders")
}

fn err(r: &RecurrenceRule) -> String {
    render_recurrence(r, start())
        .expect_err("should refuse")
        .to_string()
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

#[test]
fn a_daily_rule_renders_the_daily_pattern() {
    let out = render(&rule(Frequency::Daily));
    assert_eq!(out["pattern"]["type"], "daily");
    assert_eq!(out["pattern"]["interval"], 1);
    assert_eq!(out["range"]["type"], "noEnd");
    assert_eq!(out["range"]["startDate"], "2026-09-07");
}

#[test]
fn a_weekly_rule_names_its_days_and_week_start() {
    let mut r = rule(Frequency::Weekly);
    r.by_day = vec![nday(Weekday::Mo, None), nday(Weekday::We, None)];
    r.first_day_of_week = Weekday::Su;
    let out = render(&r);
    assert_eq!(out["pattern"]["type"], "weekly");
    assert_eq!(
        out["pattern"]["daysOfWeek"],
        serde_json::json!(["monday", "wednesday"])
    );
    assert_eq!(out["pattern"]["firstDayOfWeek"], "sunday");
}

#[test]
fn a_weekly_rule_with_no_byday_takes_the_start_dates_own_weekday() {
    // `FREQ=WEEKLY` alone means "the weekday DTSTART falls on"; Graph needs it spelled out.
    let out = render(&rule(Frequency::Weekly));
    assert_eq!(out["pattern"]["daysOfWeek"], serde_json::json!(["monday"]));
}

#[test]
fn the_start_dates_weekday_is_computed_correctly_across_a_century_boundary() {
    // The civil-date weekday maths is the one piece here with no external oracle, so pin
    // dates whose weekday is independently known.
    for (y, m, d, expected) in [
        (2026, 9, 7, Weekday::Mo),
        (2026, 9, 13, Weekday::Su),
        (2000, 2, 29, Weekday::Tu),
        (1999, 12, 31, Weekday::Fr),
        (2026, 1, 1, Weekday::Th),
        (2024, 2, 29, Weekday::Th),
    ] {
        assert_eq!(
            weekday_of(CalendarDate::new(y, m, d).unwrap()),
            expected,
            "{y}-{m:02}-{d:02}"
        );
    }
}

#[test]
fn a_monthly_rule_renders_absolute_or_relative() {
    let mut by_day = rule(Frequency::Monthly);
    by_day.by_day = vec![nday(Weekday::Mo, Some(4))];
    let out = render(&by_day);
    assert_eq!(out["pattern"]["type"], "relativeMonthly");
    assert_eq!(out["pattern"]["index"], "fourth");

    let mut last_friday = rule(Frequency::Monthly);
    last_friday.by_day = vec![nday(Weekday::Fr, Some(-1))];
    assert_eq!(render(&last_friday)["pattern"]["index"], "last");

    let mut by_month_day = rule(Frequency::Monthly);
    by_month_day.by_month_day = vec![15];
    let out = render(&by_month_day);
    assert_eq!(out["pattern"]["type"], "absoluteMonthly");
    assert_eq!(out["pattern"]["dayOfMonth"], 15);

    // No BY* part at all: the day of the month DTSTART falls on.
    let out = render(&rule(Frequency::Monthly));
    assert_eq!(out["pattern"]["type"], "absoluteMonthly");
    assert_eq!(out["pattern"]["dayOfMonth"], 7);
}

#[test]
fn a_yearly_rule_names_its_month() {
    let out = render(&rule(Frequency::Yearly));
    assert_eq!(out["pattern"]["type"], "absoluteYearly");
    assert_eq!(out["pattern"]["month"], 9);
    assert_eq!(out["pattern"]["dayOfMonth"], 7);

    let mut relative = rule(Frequency::Yearly);
    relative.by_month = vec!["3".to_owned()];
    relative.by_day = vec![nday(Weekday::Th, Some(2))];
    let out = render(&relative);
    assert_eq!(out["pattern"]["type"], "relativeYearly");
    assert_eq!(out["pattern"]["month"], 3);
    assert_eq!(out["pattern"]["index"], "second");
}

// ---------------------------------------------------------------------------
// Ranges
// ---------------------------------------------------------------------------

#[test]
fn each_bound_renders_its_range_type() {
    let mut counted = rule(Frequency::Daily);
    counted.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());
    let out = render(&counted);
    assert_eq!(out["range"]["type"], "numbered");
    assert_eq!(out["range"]["numberOfOccurrences"], 6);

    let mut until = rule(Frequency::Daily);
    until.bound = RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    let out = render(&until);
    assert_eq!(out["range"]["type"], "endDate");
    assert_eq!(out["range"]["endDate"], "2026-10-26");
}

// ---------------------------------------------------------------------------
// What Graph cannot say — refused, never approximated
// ---------------------------------------------------------------------------

#[test]
fn a_sub_daily_frequency_is_refused() {
    for frequency in [Frequency::Hourly, Frequency::Minutely, Frequency::Secondly] {
        assert!(err(&rule(frequency)).contains("sub-daily"));
    }
}

#[test]
fn an_ordinal_on_a_weekly_rule_is_refused() {
    let mut r = rule(Frequency::Weekly);
    r.by_day = vec![nday(Weekday::Mo, Some(2))];
    assert!(err(&r).contains("ordinal weekday on a weekly rule"));
}

#[test]
fn a_monthly_weekday_rule_with_no_ordinal_is_refused() {
    // `FREQ=MONTHLY;BYDAY=MO` is "every Monday of the month" — Graph's relativeMonthly
    // always picks one, so rendering it would silently narrow the series to four dates.
    let mut r = rule(Frequency::Monthly);
    r.by_day = vec![nday(Weekday::Mo, None)];
    assert!(err(&r).contains("no ordinal"));
}

#[test]
fn mixed_ordinals_are_refused_because_graph_carries_one_index() {
    let mut r = rule(Frequency::Monthly);
    r.by_day = vec![nday(Weekday::Mo, Some(1)), nday(Weekday::Fr, Some(-1))];
    assert!(err(&r).contains("different ordinals"));
}

#[test]
fn an_unindexable_ordinal_is_refused() {
    let mut r = rule(Frequency::Monthly);
    r.by_day = vec![nday(Weekday::Mo, Some(5))];
    assert!(err(&r).contains("indexes only"));
}

#[test]
fn a_negative_or_multiple_day_of_month_is_refused() {
    let mut last_day = rule(Frequency::Monthly);
    last_day.by_month_day = vec![-1];
    assert!(err(&last_day).contains("end of the month"));

    let mut two_days = rule(Frequency::Monthly);
    two_days.by_month_day = vec![1, 15];
    assert!(err(&two_days).contains("more than one BYMONTHDAY"));
}

#[test]
fn a_daily_rule_naming_weekdays_is_refused() {
    let mut r = rule(Frequency::Daily);
    r.by_day = vec![nday(Weekday::Mo, None)];
    assert!(err(&r).contains("daily rule that names weekdays"));
}

#[test]
fn every_by_part_graph_cannot_hold_is_refused_by_name() {
    // Each of these has no field in any Graph pattern, so rendering the rule without it
    // would put a different series on the calendar. The refusal names the part, so the
    // product core can say which one it was.
    for part in [
        "BYSETPOS",
        "BYWEEKNO",
        "BYYEARDAY",
        "BYHOUR",
        "BYMINUTE",
        "BYSECOND",
    ] {
        let mut r = rule(Frequency::Weekly);
        match part {
            "BYSETPOS" => r.by_set_position = vec![-1],
            "BYWEEKNO" => r.by_week_no = vec![3],
            "BYYEARDAY" => r.by_year_day = vec![100],
            "BYHOUR" => r.by_hour = vec![9],
            "BYMINUTE" => r.by_minute = vec![30],
            _ => r.by_second = vec![0],
        }
        assert!(
            err(&r).contains(part),
            "{part} should be named in the refusal"
        );
    }
}

#[test]
fn a_non_gregorian_rule_is_refused() {
    let mut r = rule(Frequency::Yearly);
    r.rscale = Some("HEBREW".to_owned());
    assert!(err(&r).contains("RSCALE=HEBREW"));
}

// ---------------------------------------------------------------------------
// Round-trip through the reader
// ---------------------------------------------------------------------------

/// `parse_recurrence(render(rule)) == rule`, for the rules Graph can express.
fn round_trips(r: &RecurrenceRule) {
    let event = serde_json::json!({ "recurrence": render(r) });
    let back: Recurrence = parse_recurrence(&event)
        .expect("parses")
        .expect("is recurring");
    assert_eq!(back.rules.len(), 1);
    assert_eq!(&back.rules[0], r, "round trip changed the rule");
}

#[test]
fn the_ux_presets_round_trip_through_graph() {
    let mut weekly = rule(Frequency::Weekly);
    weekly.by_day = vec![nday(Weekday::Mo, None)];
    round_trips(&weekly);

    let mut every_weekday = rule(Frequency::Weekly);
    every_weekday.by_day = vec![
        nday(Weekday::Mo, None),
        nday(Weekday::Tu, None),
        nday(Weekday::We, None),
        nday(Weekday::Th, None),
        nday(Weekday::Fr, None),
    ];
    round_trips(&every_weekday);

    let mut fourth_monday = rule(Frequency::Monthly);
    fourth_monday.by_day = vec![nday(Weekday::Mo, Some(4))];
    round_trips(&fourth_monday);

    let mut yearly = rule(Frequency::Yearly);
    yearly.by_month = vec!["9".to_owned()];
    yearly.by_month_day = vec![7];
    round_trips(&yearly);
}

#[test]
fn every_bound_round_trips_through_graph() {
    let mut counted = rule(Frequency::Daily);
    counted.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());
    round_trips(&counted);

    round_trips(&rule(Frequency::Daily));

    // `endDate` is date-granular both ways: the renderer takes the UNTIL's date and the
    // reader puts back that day's 23:59:59, so only a rule already bounded at end-of-day
    // survives unchanged. That is Graph's model, not a rounding bug — pinned so a future
    // change to either side has to notice.
    let mut until = rule(Frequency::Daily);
    until.bound = RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    round_trips(&until);

    let mut midday = rule(Frequency::Daily);
    midday.bound = RecurrenceBound::Until("2026-10-26T12:00:00".parse().unwrap());
    let event = serde_json::json!({ "recurrence": render(&midday) });
    let back = parse_recurrence(&event).unwrap().unwrap();
    assert_eq!(
        back.rules[0].bound,
        RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap()),
        "Graph's endDate is a date, so a mid-day UNTIL widens to the end of that day"
    );
}

#[test]
fn the_interval_and_week_start_round_trip() {
    let mut r = rule(Frequency::Weekly);
    r.interval = NonZeroU32::new(3).unwrap();
    r.by_day = vec![nday(Weekday::We, None)];
    r.first_day_of_week = Weekday::Su;
    round_trips(&r);
}
