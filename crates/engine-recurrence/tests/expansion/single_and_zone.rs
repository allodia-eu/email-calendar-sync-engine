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

/// `to_local` is the read-side inverse of expansion: it gives the wall clock an
/// instant shows as in a zone, which is how a grid picks an occurrence's day column
/// and its row.
///
/// Working from the wall clock is what makes a DST day render right. On the
/// spring-forward Sunday, 12:00 local is only 600 *real* minutes after local midnight
/// (the 02:00 hour never happened) — but it belongs on the 12:00 row, not the 10:00 one.
#[test]
fn to_local_gives_the_wall_clock_an_instant_shows_as() {
    let ams = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    // Winter (UTC+1) and summer (UTC+2): the same UTC hour reads differently.
    assert_eq!(
        to_local(instant("2026-01-15T09:00:00Z"), &ams).unwrap(),
        ldt("2026-01-15T10:00:00")
    );
    assert_eq!(
        to_local(instant("2026-07-15T09:00:00Z"), &ams).unwrap(),
        ldt("2026-07-15T11:00:00")
    );

    // Noon on the spring-forward day (2026-03-29): 10:00Z is 12:00 local. A grid that
    // counted elapsed minutes from local midnight would put this at 11:00.
    assert_eq!(
        to_local(instant("2026-03-29T10:00:00Z"), &ams).unwrap(),
        ldt("2026-03-29T12:00:00")
    );
    // Both sides of the fall-back repeated hour map to the same wall clock, an hour apart.
    assert_eq!(
        to_local(instant("2026-10-25T00:30:00Z"), &ams).unwrap(),
        ldt("2026-10-25T02:30:00")
    );
    assert_eq!(
        to_local(instant("2026-10-25T01:30:00Z"), &ams).unwrap(),
        ldt("2026-10-25T02:30:00")
    );

    // A custom/embedded VTIMEZONE the bundled tzdb cannot resolve is an error, not a guess.
    let custom = TimeZoneId::custom("X-CUSTOM").unwrap();
    assert!(to_local(instant("2026-01-15T09:00:00Z"), &custom).is_err());
}

/// `day_bounds_utc` gives the UTC window a local day occupies — the window a grid
/// queries occurrences for.
///
/// A day is **not** always 24 hours. Adding an absolute 24h to local midnight would
/// clip an hour off the spring-forward day (hiding that day's last hour of events) and
/// overrun the fall-back one (pulling the next day's first hour in).
#[test]
fn day_bounds_track_dst_so_a_day_is_not_always_24_hours() {
    let ams = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let day = |y, m, d| CalendarDate::new(y, m, d).unwrap();

    // An ordinary summer day: 24 hours, opening at 22:00Z the evening before (UTC+2).
    let ordinary = day_bounds_utc(day(2026, 7, 15), &ams).unwrap();
    assert_eq!(ordinary.start(), instant("2026-07-14T22:00:00Z"));
    assert_eq!(ordinary.end(), instant("2026-07-15T22:00:00Z"));

    // Spring forward (2026-03-29): the clocks jump 02:00 → 03:00, so the day is 23 hours.
    let short = day_bounds_utc(day(2026, 3, 29), &ams).unwrap();
    assert_eq!(short.start(), instant("2026-03-28T23:00:00Z"));
    assert_eq!(short.end(), instant("2026-03-29T22:00:00Z"));

    // Fall back (2026-10-25): 03:00 → 02:00 repeats an hour, so the day is 25 hours.
    let long = day_bounds_utc(day(2026, 10, 25), &ams).unwrap();
    assert_eq!(long.start(), instant("2026-10-24T22:00:00Z"));
    assert_eq!(long.end(), instant("2026-10-25T23:00:00Z"));
}
