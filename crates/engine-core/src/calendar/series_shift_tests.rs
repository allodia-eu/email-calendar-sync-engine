//! Where a series' occurrences go when its start moves, case by case.

use super::{Frequency, NDay, RecurrenceRule, SeriesShift, Weekday};
use crate::time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId};

fn at(text: &str) -> LocalDateTime {
    text.parse().unwrap()
}

fn amsterdam(text: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: at(text),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn weekly() -> RecurrenceRule {
    RecurrenceRule::new(Frequency::Weekly)
}

fn on_wednesdays() -> RecurrenceRule {
    let mut rule = weekly();
    rule.by_day = vec![NDay {
        day: Weekday::We,
        nth_of_period: None,
    }];
    rule
}

fn at_nine() -> RecurrenceRule {
    let mut rule = weekly();
    rule.by_hour = vec![9];
    rule
}

#[test]
fn a_later_start_moves_every_slot_by_the_same_wall_clock() {
    let shift = SeriesShift::between(
        &amsterdam("2026-06-01T09:30:00"),
        &amsterdam("2026-06-01T11:00:00"),
        &[weekly()],
    )
    .unwrap();
    assert_eq!(
        shift.slot(at("2026-06-08T09:30:00")),
        Some(at("2026-06-08T11:00:00"))
    );
}

#[test]
fn a_start_moved_a_day_and_back_an_hour_moves_the_slots_the_same() {
    // 23:30 Monday to 00:30 Tuesday is one hour later, not a day and a negative day's worth.
    let shift = SeriesShift::between(
        &amsterdam("2026-06-01T23:30:00"),
        &amsterdam("2026-06-02T00:30:00"),
        &[weekly()],
    )
    .unwrap();
    assert_eq!(
        shift.slot(at("2026-06-08T23:30:00")),
        Some(at("2026-06-09T00:30:00"))
    );
}

#[test]
fn a_rule_that_names_its_days_keeps_them() {
    // The rule still falls on Wednesdays wherever the start moves, so only the time moves.
    let shift = SeriesShift::between(
        &amsterdam("2026-06-03T09:30:00"),
        &amsterdam("2026-06-04T10:00:00"),
        &[on_wednesdays()],
    )
    .unwrap();
    assert_eq!(
        shift.slot(at("2026-06-10T09:30:00")),
        Some(at("2026-06-10T10:00:00"))
    );
}

#[test]
fn a_rule_that_names_its_hours_keeps_them() {
    let shift = SeriesShift::between(
        &amsterdam("2026-06-01T09:00:00"),
        &amsterdam("2026-06-02T09:30:00"),
        &[at_nine()],
    )
    .unwrap();
    assert_eq!(
        shift.slot(at("2026-06-08T09:00:00")),
        Some(at("2026-06-09T09:00:00"))
    );
}

#[test]
fn nothing_moves_when_the_start_does_not_or_the_rule_names_what_moved() {
    let start = amsterdam("2026-06-03T09:30:00");
    assert_eq!(SeriesShift::between(&start, &start, &[weekly()]), None);
    assert_eq!(
        SeriesShift::between(
            &start,
            &amsterdam("2026-06-10T09:30:00"),
            &[on_wednesdays()]
        ),
        None,
        "a week later on a Wednesday rule moves no slot"
    );
}

#[test]
fn a_change_of_form_moves_nothing() {
    // No write may change a start's form, so there is no shift to name across one.
    assert_eq!(
        SeriesShift::between(
            &amsterdam("2026-06-01T09:30:00"),
            &CalendarDateTime::Floating(at("2026-06-01T11:00:00")),
            &[weekly()],
        ),
        None
    );
}

#[test]
fn a_date_moves_by_days_alone() {
    let date = |text: &str| CalendarDateTime::Date(text.parse::<CalendarDate>().unwrap());
    let all_day =
        SeriesShift::between(&date("2026-06-01"), &date("2026-06-03"), &[weekly()]).unwrap();
    assert_eq!(all_day.value(&date("2026-06-08")), Some(date("2026-06-10")));

    // A DATE-valued exclusion on a timed series names a day; its time is the series' own.
    let timed = SeriesShift::between(
        &amsterdam("2026-06-01T09:30:00"),
        &amsterdam("2026-06-02T11:00:00"),
        &[weekly()],
    )
    .unwrap();
    assert_eq!(timed.value(&date("2026-06-08")), Some(date("2026-06-09")));
    assert_eq!(
        timed.value(&amsterdam("2026-06-08T09:30:00")),
        Some(amsterdam("2026-06-09T11:00:00")),
        "a timed value keeps its zone and moves by both"
    );
}
