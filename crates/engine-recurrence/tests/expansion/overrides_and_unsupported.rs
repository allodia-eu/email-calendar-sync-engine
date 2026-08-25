//! Override / exclusion / cancellation handling, standalone override instances,
//! and rejection of RRULE features outside the supported subset.

use core::num::NonZeroI32;

use engine_core::{
    calendar::{NDay, Recurrence, RecurrenceOverride, Weekday},
    patch::PatchObject,
};
use engine_recurrence::ExpandError;
use serde_json::json;

use super::*;

// --- overrides / exclusions / cancellation -------------------------------

#[test]
fn excluded_instance_is_dropped() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(3);
    rec.overrides
        .insert(ldt("2026-06-09T09:00:00"), RecurrenceOverride::Excluded);
    ev.recurrence = Some(rec);
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        ["2026-06-02T09:00:00Z", "2026-06-16T09:00:00Z"]
    );
}

#[test]
fn cancelled_override_drops_the_instance() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(2);
    rec.overrides.insert(
        ldt("2026-06-09T09:00:00"),
        RecurrenceOverride::Patch(
            PatchObject::new([("status".to_owned(), json!("cancelled"))]).unwrap(),
        ),
    );
    ev.recurrence = Some(rec);
    assert_eq!(starts(&expand_ok(&ev, wide())), ["2026-06-02T09:00:00Z"]);
}

#[test]
fn moved_instance_keeps_recurrence_id_and_uses_new_start() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(2);
    rec.overrides.insert(
        ldt("2026-06-09T09:00:00"),
        RecurrenceOverride::Patch(
            PatchObject::new([("start".to_owned(), json!("2026-06-09T14:00:00"))]).unwrap(),
        ),
    );
    ev.recurrence = Some(rec);
    let occs = expand_ok(&ev, wide());
    let moved = occs
        .iter()
        .find(|o| o.recurrence_id.is_some())
        .expect("a moved instance");
    assert_eq!(
        moved.recurrence_id.map(|i| i.to_string()).as_deref(),
        Some("2026-06-09T09:00:00Z")
    );
    assert_eq!(moved.start.to_string(), "2026-06-09T14:00:00Z");
}

#[test]
fn override_on_a_non_rule_instant_adds_an_instance() {
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(1);
    // An RDATE-like extra instance the rule did not generate.
    rec.overrides.insert(
        ldt("2026-06-05T09:00:00"),
        RecurrenceOverride::Patch(PatchObject::default()),
    );
    ev.recurrence = Some(rec);
    assert_eq!(
        starts(&expand_ok(&ev, wide())),
        ["2026-06-02T09:00:00Z", "2026-06-05T09:00:00Z"]
    );
}

#[test]
fn standalone_override_instance_event_expands_to_one_occurrence() {
    // An override-instance object (its `recurrence_id` set, no `recurrence`).
    let mut ev = event(utc("2026-06-09T14:00:00"));
    ev.recurrence_id = Some(utc("2026-06-09T09:00:00"));
    let occs = expand_ok(&ev, wide());
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].start.to_string(), "2026-06-09T14:00:00Z");
    assert_eq!(
        occs[0].recurrence_id.map(|i| i.to_string()).as_deref(),
        Some("2026-06-09T09:00:00Z")
    );
}

// --- unsupported rules ----------------------------------------------------

#[test]
fn sub_daily_frequency_is_unsupported() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    ev.recurrence = Some(Recurrence::from_rule(rule(Frequency::Hourly)));
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::UnsupportedRule(_))
    ));
}

#[test]
fn rscale_is_unsupported_not_expanded() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    let mut r = rule(Frequency::Yearly);
    r.rscale = Some("chinese".to_owned());
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::UnsupportedRule(_))
    ));
}

#[test]
fn by_set_position_is_unsupported() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    let mut r = rule(Frequency::Monthly);
    r.by_set_position = vec![1];
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::UnsupportedRule(_))
    ));
}

#[test]
fn other_unsupported_by_parts_are_rejected() {
    for mutate in [
        (|r: &mut RecurrenceRule| r.by_year_day = vec![100]) as fn(&mut RecurrenceRule),
        |r: &mut RecurrenceRule| r.by_week_no = vec![3],
        |r: &mut RecurrenceRule| r.by_hour = vec![9],
        |r: &mut RecurrenceRule| r.by_minute = vec![30],
        |r: &mut RecurrenceRule| r.by_second = vec![0],
    ] {
        let mut ev = event(utc("2026-06-01T09:00:00"));
        let mut r = rule(Frequency::Daily);
        mutate(&mut r);
        ev.recurrence = Some(Recurrence::from_rule(r));
        assert!(matches!(
            expand(&ev, &wide(), &host()),
            Err(ExpandError::UnsupportedRule(_))
        ));
    }
}

#[test]
fn nth_byday_requires_monthly_or_yearly() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    let mut r = rule(Frequency::Weekly);
    r.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: Some(NonZeroI32::new(1).unwrap()),
    }];
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::UnsupportedRule(_))
    ));
}

#[test]
fn year_relative_nth_byday_without_by_month_is_unsupported() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    let mut r = rule(Frequency::Yearly);
    r.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: Some(NonZeroI32::new(20).unwrap()),
    }];
    ev.recurrence = Some(Recurrence::from_rule(r));
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::UnsupportedRule(_))
    ));
}

#[test]
fn by_month_malformed_or_out_of_range_is_rejected() {
    let mut bad_value = event(utc("2026-03-01T09:00:00"));
    let mut r = rule(Frequency::Yearly);
    r.by_month = vec!["13".to_owned()];
    bad_value.recurrence = Some(Recurrence::from_rule(r));
    assert!(matches!(
        expand(&bad_value, &wide(), &host()),
        Err(ExpandError::OutOfRange)
    ));

    let mut malformed = event(utc("2026-03-01T09:00:00"));
    let mut r = rule(Frequency::Yearly);
    r.by_month = vec!["spring".to_owned()];
    malformed.recurrence = Some(Recurrence::from_rule(r));
    assert!(matches!(
        expand(&malformed, &wide(), &host()),
        Err(ExpandError::UnsupportedRule(_))
    ));
}

#[test]
fn daily_filtered_by_month_day_and_by_day() {
    // DAILY with BYMONTHDAY=-1 keeps only each month's last day (start on a
    // synchronized date, since DTSTART is always emitted as the first instance).
    let mut by_md = event(utc("2026-01-31T09:00:00"));
    let mut r = rule(Frequency::Daily);
    r.by_month_day = vec![-1];
    r.bound = count(3);
    by_md.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&by_md, wide())),
        [
            "2026-01-31T09:00:00Z",
            "2026-02-28T09:00:00Z",
            "2026-03-31T09:00:00Z",
        ]
    );

    // DAILY with BYDAY=MO keeps only Mondays.
    let mut by_day = event(utc("2026-06-01T09:00:00")); // a Monday
    let mut r = rule(Frequency::Daily);
    r.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    r.bound = count(3);
    by_day.recurrence = Some(Recurrence::from_rule(r));
    assert_eq!(
        starts(&expand_ok(&by_day, wide())),
        [
            "2026-06-01T09:00:00Z",
            "2026-06-08T09:00:00Z",
            "2026-06-15T09:00:00Z",
        ]
    );
}

#[test]
fn standalone_instance_outside_horizon_is_empty() {
    let mut ev = event(utc("2026-06-09T14:00:00"));
    ev.recurrence_id = Some(utc("2026-06-09T09:00:00"));
    let horizon = Horizon::new(
        instant("2027-01-01T00:00:00Z"),
        instant("2028-01-01T00:00:00Z"),
    )
    .unwrap();
    assert!(expand_ok(&ev, horizon).is_empty());
}

#[test]
fn moved_instance_outside_horizon_is_dropped() {
    // A weekly series within the horizon, but one instance is moved past it.
    let mut ev = event(utc("2026-06-02T09:00:00"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(2);
    rec.overrides.insert(
        ldt("2026-06-09T09:00:00"),
        RecurrenceOverride::Patch(
            PatchObject::new([("start".to_owned(), json!("2030-01-01T09:00:00"))]).unwrap(),
        ),
    );
    ev.recurrence = Some(rec);
    let horizon = Horizon::new(
        instant("2026-06-01T00:00:00Z"),
        instant("2026-06-30T00:00:00Z"),
    )
    .unwrap();
    // Only the un-moved 2026-06-02 instance remains; the moved one is past the window.
    assert_eq!(starts(&expand_ok(&ev, horizon)), ["2026-06-02T09:00:00Z"]);
}

#[test]
fn an_interval_past_the_representable_span_is_an_error_not_an_abort() {
    // That such a rule produces nothing is not the point. The step it asks for is a span of
    // days outside what the date arithmetic can hold, and **building** that span panics rather
    // than failing — so one malformed RRULE from a server takes the host process with it, on a
    // read, with no write of ours involved.
    //
    // Weekly reaches the bound first, at seven days to the step; daily needs seven times more.
    let mut weekly = event(utc("2026-06-02T09:00:00"));
    let mut r = rule(Frequency::Weekly);
    r.interval = NonZeroU32::new(1_043_498).expect("non-zero");
    weekly.recurrence = Some(Recurrence::from_rule(r));

    let mut daily = event(utc("2026-06-02T09:00:00"));
    let mut r = rule(Frequency::Daily);
    r.interval = NonZeroU32::new(u32::MAX).expect("non-zero");
    daily.recurrence = Some(Recurrence::from_rule(r));

    for (what, ev) in [("weekly", weekly), ("daily", daily)] {
        assert!(
            matches!(expand(&ev, &wide(), &host()), Err(ExpandError::OutOfRange)),
            "{what}: an unrepresentable step is an error a caller can report"
        );
    }
}
