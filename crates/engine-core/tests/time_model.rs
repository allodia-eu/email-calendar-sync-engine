//! Conformance tests for the engine time model: every accessor, the canonical
//! string forms, and the error surface.

use engine_core::time::{
    CalendarDate, CalendarDateTime, Duration, LocalDateTime, SignedDuration, TimeError, TimeZoneId,
    UtcDateTime,
};

#[test]
fn local_date_time_exposes_every_component() {
    let dt = LocalDateTime::new(2021, 3, 14, 9, 26, 53).unwrap();
    assert_eq!(dt.year(), 2021);
    assert_eq!(dt.month(), 3);
    assert_eq!(dt.day(), 14);
    assert_eq!(dt.hour(), 9);
    assert_eq!(dt.minute(), 26);
    assert_eq!(dt.second(), 53);
    assert_eq!(dt.nanosecond(), 0);
}

#[test]
fn utc_date_time_exposes_every_component() {
    let dt: UtcDateTime = "2021-03-14T09:26:53.5Z".parse().unwrap();
    assert_eq!(dt.year(), 2021);
    assert_eq!(dt.month(), 3);
    assert_eq!(dt.day(), 14);
    assert_eq!(dt.hour(), 9);
    assert_eq!(dt.minute(), 26);
    assert_eq!(dt.second(), 53);
    assert_eq!(dt.nanosecond(), 500_000_000);
    assert_eq!(dt.to_string(), "2021-03-14T09:26:53.5Z");
    let made = UtcDateTime::new(2021, 3, 14, 9, 26, 53).unwrap();
    assert_eq!(made.to_string(), "2021-03-14T09:26:53Z");
}

#[test]
fn calendar_date_components_and_display() {
    let date = CalendarDate::new(2020, 2, 29).unwrap();
    assert_eq!((date.year(), date.month(), date.day()), (2020, 2, 29));
    assert_eq!(date.to_string(), "2020-02-29");
}

#[test]
fn duration_accessors_and_zero() {
    let d: Duration = "P1DT2H3M4.5S".parse().unwrap();
    assert_eq!(d.days(), 1);
    assert_eq!(d.seconds(), 2 * 3600 + 3 * 60 + 4);
    assert_eq!(d.nanoseconds(), 500_000_000);
    assert!(!d.is_zero());
    assert!(Duration::ZERO.is_zero());
    assert_eq!(Duration::ZERO.to_string(), "PT0S");
}

#[test]
fn signed_duration_magnitude_and_after() {
    let after = SignedDuration::after("PT1H".parse().unwrap());
    assert!(!after.is_before());
    assert_eq!(after.magnitude().seconds(), 3600);
}

#[test]
fn time_zone_accessors() {
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    assert_eq!(zone.as_str(), "Europe/Amsterdam");
    assert!(zone.is_iana());
    let custom = TimeZoneId::custom("/My/Embedded").unwrap();
    assert_eq!(custom.as_str(), "/My/Embedded");
    assert!(!custom.is_iana());
}

#[test]
fn calendar_date_time_all_day_carries_date() {
    let value = CalendarDateTime::Date(CalendarDate::new(2021, 6, 1).unwrap());
    assert!(value.is_all_day());
    assert!(value.local().is_none());
    assert!(value.zone().is_none());
}

#[test]
fn time_errors_render_messages() {
    // Exercise every TimeError variant's Display.
    let empty = TimeZoneId::iana("").unwrap_err();
    assert_eq!(empty, TimeError::Empty);
    assert!(empty.to_string().contains("must not be empty"));

    let out_of_range = CalendarDate::new(2021, 2, 29).unwrap_err();
    assert!(out_of_range.to_string().contains("out of range"));

    let malformed = "not-a-date".parse::<LocalDateTime>().unwrap_err();
    assert!(matches!(malformed, TimeError::Malformed { .. }));
    assert!(malformed.to_string().contains("malformed"));

    let too_precise = "2021-01-01T00:00:00.1234567891"
        .parse::<LocalDateTime>()
        .unwrap_err();
    assert_eq!(too_precise, TimeError::SubsecondTooPrecise);
    assert!(too_precise.to_string().contains("nanoseconds"));
}

#[test]
fn malformed_times_and_durations_are_rejected() {
    // Exercise every parse error branch for wall-clock times.
    for bad in [
        "2021-01-01T00x00:00",    // bad time separator
        "2021-01-01T00:00:00x12", // no '.' before fractional seconds
        "2021-01-01T00:00:00.1a", // non-digit fractional seconds
        "2021-01-01T24:00:00",    // hour out of range
        "2021-01-01x00:00:00",    // bad date/time separator
    ] {
        assert!(bad.parse::<LocalDateTime>().is_err(), "{bad}");
    }
    // Duplicate, out-of-order, and invalid duration units.
    for bad in ["P1D1W", "P1W1W", "PT1X", "PT1S1M", "PT1H1H"] {
        assert!(bad.parse::<Duration>().is_err(), "{bad}");
    }
}
