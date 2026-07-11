//! Determinism (the byte-stability precondition for a tzdata bump), empty-window
//! rejection, and nominal-day durations across a DST transition.

use engine_core::{
    calendar::Recurrence,
    time::{Duration, TimeError},
};

use super::*;

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
        Err(TimeError::EmptyRange)
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
