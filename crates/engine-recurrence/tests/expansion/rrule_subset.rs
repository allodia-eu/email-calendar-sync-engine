//! The supported RRULE subset: frequencies, intervals, bounds, BYDAY/BYMONTH
//! selectors, plus override-driven starts/durations and horizon capping.

use core::num::NonZeroI32;

use engine_core::{
    calendar::{NDay, Recurrence, RecurrenceOverride, Weekday},
    patch::PatchObject,
    time::Duration,
};
use engine_recurrence::ExpandError;
use serde_json::json;

use super::*;

#[test]
fn weekly_count_emits_exactly_count_instances() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(3);
    ev.recurrence = Some(rec);
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        [
            "2026-06-02T09:00:00Z",
            "2026-06-09T09:00:00Z",
            "2026-06-16T09:00:00Z",
        ]
    );
}

#[test]
fn daily_until_is_inclusive() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Daily));
    rec.rules[0].bound = RecurrenceBound::Until(ldt("2026-06-03T09:00:00"));
    ev.recurrence = Some(rec);
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        [
            "2026-06-01T09:00:00Z",
            "2026-06-02T09:00:00Z",
            "2026-06-03T09:00:00Z",
        ]
    );
}

#[test]
fn daily_interval_skips_periods() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    let mut r = rule(Frequency::Daily);
    r.interval = NonZeroU32::new(2).unwrap();
    r.bound = count(3);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        [
            "2026-06-01T09:00:00Z",
            "2026-06-03T09:00:00Z",
            "2026-06-05T09:00:00Z",
        ]
    );
}

#[test]
fn monthly_nth_weekday() {
    // First Monday of each month, starting Jan 2026 (Jan 5 is the first Monday).
    let mut ev = event(utc("2026-01-05T09:00:00"));
    let mut r = rule(Frequency::Monthly);
    r.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: Some(NonZeroI32::new(1).unwrap()),
    }];
    r.bound = count(3);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        [
            "2026-01-05T09:00:00Z",
            "2026-02-02T09:00:00Z",
            "2026-03-02T09:00:00Z",
        ]
    );
}

#[test]
fn monthly_last_weekday_negative_nth() {
    // Last Friday of each month.
    let mut ev = event(utc("2026-01-30T09:00:00"));
    let mut r = rule(Frequency::Monthly);
    r.by_day = vec![NDay {
        day: Weekday::Fr,
        nth_of_period: Some(NonZeroI32::new(-1).unwrap()),
    }];
    r.bound = count(2);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        ["2026-01-30T09:00:00Z", "2026-02-27T09:00:00Z"]
    );
}

#[test]
fn monthly_negative_month_day() {
    // Last day of each month adapts to month length.
    let mut ev = event(utc("2026-01-31T09:00:00"));
    let mut r = rule(Frequency::Monthly);
    r.by_month_day = vec![-1];
    r.bound = count(3);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        [
            "2026-01-31T09:00:00Z",
            "2026-02-28T09:00:00Z",
            "2026-03-31T09:00:00Z",
        ]
    );
}

#[test]
fn yearly_on_start_month_and_day() {
    let mut ev = event(utc("2026-02-15T09:00:00"));
    let mut r = rule(Frequency::Yearly);
    r.bound = count(3);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        [
            "2026-02-15T09:00:00Z",
            "2027-02-15T09:00:00Z",
            "2028-02-15T09:00:00Z",
        ]
    );
}

#[test]
fn yearly_with_by_month_expands_each_named_month() {
    let mut ev = event(utc("2026-03-10T09:00:00"));
    let mut r = rule(Frequency::Yearly);
    r.by_month = vec!["3".to_owned(), "6".to_owned()];
    r.bound = count(4);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        [
            "2026-03-10T09:00:00Z",
            "2026-06-10T09:00:00Z",
            "2027-03-10T09:00:00Z",
            "2027-06-10T09:00:00Z",
        ]
    );
}

#[test]
fn yearly_nth_weekday_within_a_month() {
    // The fourth Thursday of November (US Thanksgiving).
    let mut ev = event(utc("2026-11-26T09:00:00"));
    let mut r = rule(Frequency::Yearly);
    r.by_month = vec!["11".to_owned()];
    r.by_day = vec![NDay {
        day: Weekday::Th,
        nth_of_period: Some(NonZeroI32::new(4).unwrap()),
    }];
    r.bound = count(2);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        ["2026-11-26T09:00:00Z", "2027-11-25T09:00:00Z"]
    );
}

#[test]
fn monthly_weekday_without_nth_expands_all_in_month() {
    // Every Monday; restricted to June 2026 by the horizon.
    let mut ev = event(utc("2026-06-01T09:00:00")); // 2026-06-01 is a Monday
    let mut r = rule(Frequency::Monthly);
    r.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    ev.recurrence = Some(Recurrence::from_rule(r));
    let june = Horizon::new(
        instant("2026-06-01T00:00:00Z"),
        instant("2026-07-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&expand_ok(&ev, june)),
        [
            "2026-06-01T09:00:00Z",
            "2026-06-08T09:00:00Z",
            "2026-06-15T09:00:00Z",
            "2026-06-22T09:00:00Z",
            "2026-06-29T09:00:00Z",
        ]
    );
}

#[test]
fn monthly_byday_and_bymonthday_intersect() {
    // Friday the 13th: BYDAY=FR ∩ BYMONTHDAY=13.
    let mut ev = event(utc("2026-02-13T09:00:00")); // 2026-02-13 is a Friday
    let mut r = rule(Frequency::Monthly);
    r.by_day = vec![NDay {
        day: Weekday::Fr,
        nth_of_period: None,
    }];
    r.by_month_day = vec![13];
    r.bound = count(2);
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        ["2026-02-13T09:00:00Z", "2026-03-13T09:00:00Z"]
    );
}

#[test]
fn excluded_rules_subtract_instances() {
    // A daily series with a weekly EXRULE removes the matching weekday.
    let mut ev = event(utc("2026-06-01T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Daily));
    rec.rules[0].bound = count(7); // 2026-06-01 .. 2026-06-07
    rec.excluded_rules
        .push(RecurrenceRule::new(Frequency::Weekly)); // removes 2026-06-01
    ev.recurrence = Some(rec);
    let out = starts(&expand_ok(&ev, wide()));
    assert_eq!(out.len(), 6);
    assert!(!out.contains(&"2026-06-01T09:00:00Z".to_owned()));
    assert!(out.contains(&"2026-06-02T09:00:00Z".to_owned()));
}

#[test]
fn sub_second_start_keeps_fraction() {
    let ev = event(CalendarDateTime::utc(
        "2026-06-01T09:00:00.5".parse().unwrap(),
    ));
    let occs = expand_ok(&ev, wide());
    assert_eq!(occs[0].start.to_string(), "2026-06-01T09:00:00.5Z");
}

#[test]
fn moved_instance_can_change_zone() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(2);
    rec.overrides.insert(
        ldt("2026-06-09T09:00:00"),
        RecurrenceOverride::Patch(
            PatchObject::new([
                ("start".to_owned(), json!("2026-06-09T14:00:00")),
                ("timeZone".to_owned(), json!("America/New_York")),
            ])
            .unwrap(),
        ),
    );
    ev.recurrence = Some(rec);
    let moved = expand_ok(&ev, wide())
        .into_iter()
        .find(|o| o.recurrence_id.is_some())
        .unwrap();
    // 14:00 in New York (EDT, UTC-4) is 18:00Z.
    assert_eq!(moved.start.to_string(), "2026-06-09T18:00:00Z");
}

#[test]
fn malformed_override_start_is_rejected() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(2);
    rec.overrides.insert(
        ldt("2026-06-09T09:00:00"),
        RecurrenceOverride::Patch(
            PatchObject::new([("start".to_owned(), json!("not-a-date"))]).unwrap(),
        ),
    );
    ev.recurrence = Some(rec);
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::InvalidOverride { .. })
    ));
}

#[test]
fn malformed_override_duration_is_rejected() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(2);
    rec.overrides.insert(
        ldt("2026-06-09T09:00:00"),
        RecurrenceOverride::Patch(
            PatchObject::new([("duration".to_owned(), json!("nope"))]).unwrap(),
        ),
    );
    ev.recurrence = Some(rec);
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::InvalidOverride { .. })
    ));
}

#[test]
fn absurd_duration_is_out_of_range() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    ev.duration = Duration::from_parts(0, 4_000_000, 0, 0, 0, 0).unwrap();
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::OutOfRange)
    ));
}

#[test]
fn unbounded_daily_is_capped_by_the_horizon() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    ev.recurrence = Some(Recurrence::from_rule(rule(Frequency::Daily)));
    let horizon = Horizon::new(
        instant("2026-06-01T00:00:00Z"),
        instant("2026-06-04T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        starts(&expand_ok(&ev, horizon)),
        [
            "2026-06-01T09:00:00Z",
            "2026-06-02T09:00:00Z",
            "2026-06-03T09:00:00Z",
        ]
    );
}

#[test]
fn count_before_horizon_still_limits_the_series() {
    // COUNT counts from the series start; instances before the horizon are counted
    // but not materialized, so a series that ends before the window emits nothing.
    let mut ev = event(utc("2020-01-01T09:00:00"));
    let mut r = rule(Frequency::Daily);
    r.bound = count(5);
    ev.recurrence = Some(Recurrence::from_rule(r));
    let horizon = Horizon::new(
        instant("2026-01-01T00:00:00Z"),
        instant("2027-01-01T00:00:00Z"),
    )
    .unwrap();
    assert!(expand_ok(&ev, horizon).is_empty());
}
