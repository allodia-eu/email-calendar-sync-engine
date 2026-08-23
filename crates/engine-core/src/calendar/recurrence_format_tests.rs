//! Tests for the `RRULE` renderer, including the round-trip against the parser.

use core::num::{NonZeroI32, NonZeroU32};

use super::*;
use crate::calendar::parse_rrule;

fn rule(frequency: Frequency) -> RecurrenceRule {
    RecurrenceRule::new(frequency)
}

fn nday(day: Weekday, nth: Option<i32>) -> NDay {
    NDay {
        day,
        nth_of_period: nth.and_then(NonZeroI32::new),
    }
}

/// Renders a rule that cannot end at a wall clock, so the `UNTIL` form is irrelevant.
fn render(r: &RecurrenceRule) -> String {
    format_rrule(r, UntilForm::Floating).expect("renders")
}

#[test]
fn a_bare_rule_is_just_its_frequency() {
    assert_eq!(render(&rule(Frequency::Daily)), "FREQ=DAILY");
    assert_eq!(render(&rule(Frequency::Yearly)), "FREQ=YEARLY");
}

#[test]
fn defaults_are_omitted_so_the_output_matches_what_servers_echo() {
    // INTERVAL=1 and WKST=MO are the RFC 5545 defaults. Emitting them would make every
    // rule we write differ textually from the same rule written by any other client.
    let mut r = rule(Frequency::Weekly);
    r.interval = NonZeroU32::MIN;
    r.first_day_of_week = Weekday::Mo;
    assert_eq!(render(&r), "FREQ=WEEKLY");

    r.interval = NonZeroU32::new(2).unwrap();
    r.first_day_of_week = Weekday::Su;
    assert_eq!(render(&r), "FREQ=WEEKLY;INTERVAL=2;WKST=SU");
}

#[test]
fn the_ux_presets_render_as_expected() {
    // The four rules the product's repeat picker can produce, spelled out so a change to
    // the renderer that breaks one of them is visible here rather than on a real calendar.
    let mut weekly_on_monday = rule(Frequency::Weekly);
    weekly_on_monday.by_day = vec![nday(Weekday::Mo, None)];
    assert_eq!(render(&weekly_on_monday), "FREQ=WEEKLY;BYDAY=MO");

    let mut fourth_monday = rule(Frequency::Monthly);
    fourth_monday.by_day = vec![nday(Weekday::Mo, Some(4))];
    assert_eq!(render(&fourth_monday), "FREQ=MONTHLY;BYDAY=4MO");

    let mut last_friday = rule(Frequency::Monthly);
    last_friday.by_day = vec![nday(Weekday::Fr, Some(-1))];
    assert_eq!(render(&last_friday), "FREQ=MONTHLY;BYDAY=-1FR");

    let mut every_weekday = rule(Frequency::Weekly);
    every_weekday.by_day = vec![
        nday(Weekday::Mo, None),
        nday(Weekday::Tu, None),
        nday(Weekday::We, None),
        nday(Weekday::Th, None),
        nday(Weekday::Fr, None),
    ];
    assert_eq!(render(&every_weekday), "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR");
}

#[test]
fn a_counted_rule_renders_count() {
    let mut r = rule(Frequency::Daily);
    r.bound = RecurrenceBound::Count(NonZeroU32::new(10).unwrap());
    assert_eq!(render(&r), "FREQ=DAILY;COUNT=10");
}

#[test]
fn until_renders_in_the_form_the_series_dtstart_requires() {
    // RFC 5545 §3.3.10: a zoned or UTC DTSTART requires UNTIL in UTC; an all-day series
    // takes a DATE. Getting this wrong ends the series on the wrong day for readers in
    // another zone, which is exactly why the form is stated rather than guessed.
    let mut r = rule(Frequency::Weekly);
    r.bound = RecurrenceBound::Until(LocalDateTime::new(2026, 10, 26, 23, 59, 59).unwrap());

    assert_eq!(
        format_rrule(&r, UntilForm::Floating).unwrap(),
        "FREQ=WEEKLY;UNTIL=20261026T235959"
    );
    assert_eq!(
        format_rrule(&r, UntilForm::Date).unwrap(),
        "FREQ=WEEKLY;UNTIL=20261026"
    );
    // The zoned case: 23:59:59 in Europe/Amsterdam is 22:59:59Z on that date. The caller
    // resolved it, because this crate has no tzdata to do so.
    let instant = UtcDateTime::new(2026, 10, 26, 22, 59, 59).unwrap();
    assert_eq!(
        format_rrule(&r, UntilForm::Utc(instant)).unwrap(),
        "FREQ=WEEKLY;UNTIL=20261026T225959Z"
    );
}

#[test]
fn the_until_form_is_ignored_when_the_rule_does_not_end_at_a_wall_clock() {
    let mut r = rule(Frequency::Daily);
    r.bound = RecurrenceBound::Count(NonZeroU32::new(3).unwrap());
    let instant = UtcDateTime::new(2026, 1, 1, 0, 0, 0).unwrap();
    assert_eq!(
        format_rrule(&r, UntilForm::Utc(instant)).unwrap(),
        format_rrule(&r, UntilForm::Date).unwrap()
    );
}

#[test]
fn every_by_part_renders_in_a_stable_order() {
    let mut r = rule(Frequency::Yearly);
    r.interval = NonZeroU32::new(3).unwrap();
    r.by_second = vec![0, 30];
    r.by_minute = vec![15];
    r.by_hour = vec![9, 17];
    r.by_day = vec![nday(Weekday::Th, Some(2))];
    r.by_month_day = vec![1, -1];
    r.by_year_day = vec![100];
    r.by_week_no = vec![-2];
    r.by_month = vec!["3".to_owned(), "6".to_owned()];
    r.by_set_position = vec![-1];
    r.first_day_of_week = Weekday::Su;

    assert_eq!(
        render(&r),
        "FREQ=YEARLY;INTERVAL=3;BYSECOND=0,30;BYMINUTE=15;BYHOUR=9,17;BYDAY=2TH;\
         BYMONTHDAY=1,-1;BYYEARDAY=100;BYWEEKNO=-2;BYMONTH=3,6;BYSETPOS=-1;WKST=SU"
    );
}

#[test]
fn a_non_gregorian_rule_is_refused_rather_than_silently_degraded() {
    // Rendering without the RSCALE would turn a Hebrew-calendar yearly rule into a
    // Gregorian one — a different rule, stored as if it were the same.
    let mut r = rule(Frequency::Yearly);
    r.rscale = Some("HEBREW".to_owned());
    assert_eq!(
        format_rrule(&r, UntilForm::Floating),
        Err(RruleFormatError::NonGregorian("HEBREW".to_owned()))
    );
}

// ---------------------------------------------------------------------------
// Round-trip against the parser
// ---------------------------------------------------------------------------

/// `parse(format(rule)) == rule` for every rule the renderer accepts.
///
/// This direction is the one that matters: we own the rule and emit the string. The
/// reverse does not hold and is not meant to — the parser normalizes, so re-rendering a
/// parsed rule produces the canonical spelling rather than the original bytes.
fn round_trips(r: &RecurrenceRule) {
    let rendered = format_rrule(r, UntilForm::Floating).expect("renders");
    let parsed =
        parse_rrule(&rendered).unwrap_or_else(|e| panic!("re-parsing {rendered:?} failed: {e}"));
    assert_eq!(&parsed, r, "round trip changed the rule via {rendered:?}");
}

#[test]
fn every_frequency_round_trips() {
    for frequency in [
        Frequency::Secondly,
        Frequency::Minutely,
        Frequency::Hourly,
        Frequency::Daily,
        Frequency::Weekly,
        Frequency::Monthly,
        Frequency::Yearly,
    ] {
        round_trips(&rule(frequency));
    }
}

#[test]
fn every_weekday_and_ordinal_round_trips() {
    for day in [
        Weekday::Mo,
        Weekday::Tu,
        Weekday::We,
        Weekday::Th,
        Weekday::Fr,
        Weekday::Sa,
        Weekday::Su,
    ] {
        for nth in [None, Some(1), Some(4), Some(-1), Some(-53), Some(53)] {
            let mut r = rule(Frequency::Monthly);
            r.by_day = vec![nday(day, nth)];
            r.first_day_of_week = day;
            round_trips(&r);
        }
    }
}

#[test]
fn every_bound_round_trips() {
    let mut counted = rule(Frequency::Daily);
    counted.bound = RecurrenceBound::Count(NonZeroU32::new(99).unwrap());
    round_trips(&counted);

    let mut until = rule(Frequency::Daily);
    until.bound = RecurrenceBound::Until(LocalDateTime::new(2026, 2, 28, 8, 5, 9).unwrap());
    round_trips(&until);

    round_trips(&rule(Frequency::Daily));
}

#[test]
fn a_fully_populated_rule_round_trips() {
    let mut r = rule(Frequency::Yearly);
    r.interval = NonZeroU32::new(7).unwrap();
    r.by_second = vec![0, 59];
    r.by_minute = vec![0, 30];
    r.by_hour = vec![0, 23];
    r.by_day = vec![nday(Weekday::Mo, Some(-1)), nday(Weekday::Su, None)];
    r.by_month_day = vec![-31, 1, 31];
    r.by_year_day = vec![-366, 366];
    r.by_week_no = vec![-53, 53];
    r.by_month = vec!["1".to_owned(), "12".to_owned()];
    r.by_set_position = vec![-1, 1];
    r.first_day_of_week = Weekday::Sa;
    r.bound = RecurrenceBound::Until(LocalDateTime::new(2030, 12, 31, 23, 59, 59).unwrap());
    round_trips(&r);
}

#[test]
fn a_utc_until_round_trips_through_the_parser_as_the_same_wall_clock() {
    // The parser strips the trailing Z and reads the wall clock, so a rendered UTC UNTIL
    // parses back to the instant's own clock — not to the zone-local one it came from.
    // A caller that renders UTC must therefore expect UTC back, which is what the
    // adapters do when they re-read a stored rule.
    let mut r = rule(Frequency::Weekly);
    r.bound = RecurrenceBound::Until(LocalDateTime::new(2026, 10, 26, 22, 59, 59).unwrap());
    let instant = UtcDateTime::new(2026, 10, 26, 22, 59, 59).unwrap();
    let rendered = format_rrule(&r, UntilForm::Utc(instant)).unwrap();
    assert_eq!(parse_rrule(&rendered).unwrap(), r);
}
