//! Behavioral tests for recurrence/occurrence expansion.
//!
//! Locks the `calendar-semantics.md` "Required tests" reachable by the pure
//! expander (VTIMEZONE source, tzdata-version recording + determinism, floating vs
//! all-day) plus the supported RRULE subset and the override/exclusion/cancelled
//! rules.

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::calendar::{
    Event, EventStatus, Frequency, NDay, Recurrence, RecurrenceBound, RecurrenceOverride,
    RecurrenceRule, Weekday,
};
use engine_core::ids::{CalendarId, EventId, Uid};
use engine_core::membership::Memberships;
use engine_core::patch::PatchObject;
use engine_core::time::{CalendarDate, CalendarDateTime, Duration, LocalDateTime, TimeZoneId};
use engine_recurrence::{ExpandError, Horizon, OccurrenceRow, expand, tzdata_version};
use serde_json::json;

// --- helpers --------------------------------------------------------------

fn ldt(s: &str) -> LocalDateTime {
    s.parse().expect("valid local date-time")
}

fn instant(s: &str) -> engine_core::time::UtcDateTime {
    s.parse().expect("valid instant")
}

fn zoned(local: &str, zone: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: ldt(local),
        zone: TimeZoneId::iana(zone).expect("valid zone"),
    }
}

fn utc(local: &str) -> CalendarDateTime {
    zoned(local, "Etc/UTC")
}

/// The observer/device zone used for floating resolution (irrelevant to zoned and
/// all-day events).
fn host() -> TimeZoneId {
    TimeZoneId::utc()
}

fn event(start: CalendarDateTime) -> Event {
    Event::new(
        EventId::try_from("e1").expect("id"),
        Uid::new("u1").expect("uid"),
        Memberships::of_one(CalendarId::try_from("cal").expect("cal")),
        start,
    )
}

/// A wide horizon so COUNT/UNTIL/nth tests are never horizon-limited.
fn wide() -> Horizon {
    Horizon::new(
        instant("2000-01-01T00:00:00Z"),
        instant("2035-01-01T00:00:00Z"),
    )
    .expect("wide horizon")
}

fn rule(freq: Frequency) -> RecurrenceRule {
    RecurrenceRule::new(freq)
}

fn count(n: u32) -> RecurrenceBound {
    RecurrenceBound::Count(NonZeroU32::new(n).expect("non-zero"))
}

fn starts(occs: &[OccurrenceRow]) -> Vec<String> {
    occs.iter().map(|o| o.start.to_string()).collect()
}

fn expand_ok(ev: &Event, horizon: Horizon) -> Vec<OccurrenceRow> {
    expand(ev, &horizon, &host()).expect("expansion succeeds")
}

// --- single (non-recurring) events ---------------------------------------

#[test]
fn single_event_materializes_one_occurrence() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    ev.duration = "PT1H".parse().unwrap();
    let occs = expand_ok(&ev, wide());
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].start.to_string(), "2026-06-01T09:00:00Z");
    assert_eq!(occs[0].end.to_string(), "2026-06-01T10:00:00Z");
    assert_eq!(occs[0].recurrence_id, None);
    assert_eq!(occs[0].tzdata_version, tzdata_version());
}

#[test]
fn cancelled_event_materializes_nothing() {
    let mut ev = event(utc("2026-06-01T09:00:00"));
    ev.status = EventStatus::Cancelled;
    assert!(expand_ok(&ev, wide()).is_empty());
}

#[test]
fn event_outside_the_horizon_is_not_materialized() {
    let ev = event(utc("2026-06-01T09:00:00"));
    let horizon = Horizon::new(
        instant("2027-01-01T00:00:00Z"),
        instant("2028-01-01T00:00:00Z"),
    )
    .unwrap();
    assert!(expand_ok(&ev, horizon).is_empty());
}

// --- floating vs all-day (calendar-semantics required test) --------------

#[test]
fn floating_event_resolves_differently_per_host_zone() {
    let ev = event(CalendarDateTime::Floating(ldt("2026-06-01T12:00:00")));
    let ams = expand(&ev, &wide(), &TimeZoneId::iana("Europe/Amsterdam").unwrap()).unwrap();
    let nyc = expand(&ev, &wide(), &TimeZoneId::iana("America/New_York").unwrap()).unwrap();
    // 12:00 wall-clock is UTC+2 in Amsterdam (summer) and UTC-4 in New York.
    assert_eq!(ams[0].start.to_string(), "2026-06-01T10:00:00Z");
    assert_eq!(nyc[0].start.to_string(), "2026-06-01T16:00:00Z");
}

#[test]
fn all_day_event_is_zone_invariant() {
    let ev = event(CalendarDateTime::Date(
        CalendarDate::new(2026, 6, 1).unwrap(),
    ));
    let ams = expand(&ev, &wide(), &TimeZoneId::iana("Europe/Amsterdam").unwrap()).unwrap();
    let nyc = expand(&ev, &wide(), &TimeZoneId::iana("America/New_York").unwrap()).unwrap();
    assert_eq!(ams[0].start.to_string(), "2026-06-01T00:00:00Z");
    assert_eq!(nyc[0].start, ams[0].start);
}

// --- IANA zone resolution + DST (VTIMEZONE-source required test) ---------

#[test]
fn weekly_series_crosses_dst_using_iana_rules() {
    // A zoned event uses IANA tzdata (not any embedded VTIMEZONE): 09:00 Amsterdam
    // is 08:00Z under CET and 07:00Z under CEST. The spring-forward (2026-03-29)
    // falls between the first and second instance.
    let mut ev = event(zoned("2026-03-24T09:00:00", "Europe/Amsterdam"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(3);
    ev.recurrence = Some(rec);
    let occs = expand_ok(&ev, wide());
    assert_eq!(
        starts(&occs),
        [
            "2026-03-24T08:00:00Z",
            "2026-03-31T07:00:00Z",
            "2026-04-07T07:00:00Z",
        ]
    );
}

#[test]
fn custom_zone_is_unsupported() {
    let ev = event(CalendarDateTime::Zoned {
        local: ldt("2026-06-01T09:00:00"),
        zone: TimeZoneId::custom("Made/Up").unwrap(),
    });
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::UnsupportedZone(_))
    ));
}

#[test]
fn unknown_iana_zone_is_unsupported() {
    let ev = event(zoned("2026-06-01T09:00:00", "Mars/Olympus_Mons"));
    assert!(matches!(
        expand(&ev, &wide(), &host()),
        Err(ExpandError::UnsupportedZone(_))
    ));
}

// --- supported RRULE subset ----------------------------------------------

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

// --- determinism (tzdata-bump byte-stability precondition) ---------------

#[test]
fn expansion_is_deterministic() {
    let mut ev = event(zoned("2026-03-24T09:00:00", "Europe/Amsterdam"));
    let mut rec = Recurrence::from_rule(rule(Frequency::Weekly));
    rec.rules[0].bound = count(5);
    ev.recurrence = Some(rec);
    assert_eq!(expand_ok(&ev, wide()), expand_ok(&ev, wide()));
}

#[test]
fn horizon_rejects_empty_window() {
    assert!(matches!(
        Horizon::new(
            instant("2026-01-01T00:00:00Z"),
            instant("2026-01-01T00:00:00Z"),
        ),
        Err(ExpandError::EmptyHorizon)
    ));
}

#[test]
fn duration_with_nominal_days_spans_dst() {
    // A 1-day nominal duration over the spring-forward keeps the wall clock, so the
    // UTC end is 23h after the start, not 24h.
    let mut ev = event(zoned("2026-03-28T09:00:00", "Europe/Amsterdam"));
    ev.duration = Duration::from_parts(0, 1, 0, 0, 0, 0).unwrap();
    let occ = &expand_ok(&ev, wide())[0];
    assert_eq!(occ.start.to_string(), "2026-03-28T08:00:00Z"); // CET, UTC+1
    assert_eq!(occ.end.to_string(), "2026-03-29T07:00:00Z"); // next day 09:00 CEST, UTC+2
}
