//! Single (non-recurring) events, floating vs all-day resolution, and IANA
//! zone/DST handling for the pure expander.

use engine_core::{
    calendar::{EventStatus, Recurrence},
    time::CalendarDate,
};
use engine_recurrence::{ExpandError, tzdata_version};

use super::*;

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
