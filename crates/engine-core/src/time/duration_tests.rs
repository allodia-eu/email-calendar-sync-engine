//! Parsing, formatting and arithmetic for the ISO-8601 durations.
//!
//! Its own file for the 500-line limit: `duration.rs` states the type, this states what
//! the type has to survive.

use super::*;

#[test]
fn a_std_duration_lands_wholly_in_the_absolute_component() {
    // An HTTP `Retry-After`, or a quota window an adapter measured, is an exact span
    // of seconds: there is no nominal day in it to preserve, and putting one there
    // would make "come back in 86400 seconds" survive a DST boundary differently from
    // the way the server meant.
    let span = Duration::from(core::time::Duration::new(90_061, 250));
    assert_eq!(span.days(), 0);
    assert_eq!(span.seconds(), 90_061);
    assert_eq!(span.nanoseconds(), 250);
    assert!(Duration::from(core::time::Duration::ZERO).is_zero());
}

#[test]
fn from_parts_folds_weeks_and_time() {
    let d = Duration::from_parts(1, 2, 3, 4, 5, 0).unwrap();
    assert_eq!(d.days(), 9); // 1 week + 2 days
    assert_eq!(d.seconds(), 3 * 3600 + 4 * 60 + 5);
    assert!(!d.is_zero());
}

#[test]
fn from_parts_rejects_bad_nanoseconds() {
    assert_eq!(
        Duration::from_parts(0, 0, 0, 0, 0, 1_000_000_000),
        Err(TimeError::OutOfRange)
    );
}

#[test]
fn from_parts_rejects_overflow() {
    // Each component can overflow `u64` when folded; all map to OutOfRange.
    assert_eq!(
        Duration::from_parts(u64::MAX, 0, 0, 0, 0, 0),
        Err(TimeError::OutOfRange)
    );
    assert_eq!(
        Duration::from_parts(0, 0, u64::MAX, 0, 0, 0),
        Err(TimeError::OutOfRange)
    );
    assert_eq!(
        Duration::from_parts(0, 0, 0, u64::MAX, 0, 0),
        Err(TimeError::OutOfRange)
    );
    assert_eq!(
        Duration::from_parts(0, 0, 1, 0, u64::MAX, 0),
        Err(TimeError::OutOfRange)
    );
}

#[test]
fn parse_and_display_roundtrip() {
    for (text, days, seconds, nanos) in [
        ("PT0S", 0, 0, 0),
        ("P9DT3H4M5S", 9, 3 * 3600 + 4 * 60 + 5, 0),
        ("PT1H", 0, 3600, 0),
        ("PT30M", 0, 1800, 0),
        ("P7D", 7, 0, 0),
        ("PT0.5S", 0, 0, 500_000_000),
        ("PT1M30S", 0, 90, 0),
    ] {
        let d: Duration = text.parse().unwrap();
        assert_eq!(
            (d.days(), d.seconds(), d.nanoseconds()),
            (days, seconds, nanos),
            "{text}"
        );
        assert_eq!(d.to_string(), text, "roundtrip {text}");
    }
}

#[test]
fn parse_folds_weeks_to_days_in_display() {
    let d: Duration = "P2W".parse().unwrap();
    assert_eq!(d.days(), 14);
    assert_eq!(d.to_string(), "P14D");
}

#[test]
fn parse_rejects_malformed_durations() {
    for bad in [
        "", "P", "PT", "1H", "P1H", "PT5S1H", "PT1H1H", "P-1D", "PTS",
    ] {
        assert!(bad.parse::<Duration>().is_err(), "should reject {bad:?}");
    }
}

#[test]
fn signed_duration_before_and_after() {
    let fifteen_min: Duration = "PT15M".parse().unwrap();
    let before = SignedDuration::before(fifteen_min);
    assert!(before.is_before());
    assert_eq!(before.to_string(), "-PT15M");

    let after: SignedDuration = "PT15M".parse().unwrap();
    assert!(!after.is_before());
    assert_eq!(after.to_string(), "PT15M");

    // Leading `+` parses as positive.
    assert_eq!("+PT15M".parse::<SignedDuration>().unwrap(), after);
}

#[test]
fn signed_zero_has_single_representation() {
    let zero = SignedDuration::before(Duration::ZERO);
    assert!(!zero.is_before());
    assert_eq!(zero.to_string(), "PT0S");
    assert_eq!(
        SignedDuration::before(Duration::ZERO),
        SignedDuration::after(Duration::ZERO)
    );
}

#[test]
fn serde_roundtrips_as_strings() {
    let d: Duration = "P1DT2H".parse().unwrap();
    assert_eq!(serde_json::to_string(&d).unwrap(), "\"P1DT2H\"");
    assert_eq!(serde_json::from_str::<Duration>("\"P1DT2H\"").unwrap(), d);

    let s = SignedDuration::before("PT10M".parse().unwrap());
    assert_eq!(serde_json::to_string(&s).unwrap(), "\"-PT10M\"");
    assert_eq!(
        serde_json::from_str::<SignedDuration>("\"-PT10M\"").unwrap(),
        s
    );
}
