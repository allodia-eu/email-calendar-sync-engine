//! Tests for the shared override-patch builder.

use super::*;
use crate::time::{CalendarDate, TimeZoneId};

fn patch(over: RecurrenceOverride) -> PatchObject {
    match over {
        RecurrenceOverride::Patch(patch) => patch,
        RecurrenceOverride::Excluded => panic!("expected a patch"),
    }
}

fn zoned(local: &str, zone: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana(zone).unwrap(),
    }
}

#[test]
fn a_moved_occurrence_carries_its_wall_clock_and_the_zone_that_reads_it() {
    let built = patch(
        OverrideBuilder::new()
            .start(&zoned("2026-06-08T14:00:00", "Europe/Amsterdam"))
            .duration(Duration::from_parts(0, 0, 0, 45, 0, 0).unwrap())
            .build()
            .unwrap(),
    );
    assert_eq!(
        built.get("start").and_then(Value::as_str),
        Some("2026-06-08T14:00:00")
    );
    assert_eq!(
        built.get("timeZone").and_then(Value::as_str),
        Some("Europe/Amsterdam")
    );
    assert_eq!(built.get("duration").and_then(Value::as_str), Some("PT45M"));
}

#[test]
fn an_all_day_start_contributes_midnight_and_no_zone() {
    // The expander keys an all-day series at midnight and resolves it in UTC, so a zone
    // here would be a claim the model does not make.
    let built = patch(
        OverrideBuilder::new()
            .start(&CalendarDateTime::Date(
                CalendarDate::new(2026, 6, 8).unwrap(),
            ))
            .build()
            .unwrap(),
    );
    assert_eq!(
        built.get("start").and_then(Value::as_str),
        Some("2026-06-08T00:00:00")
    );
    assert!(built.get("timeZone").is_none());
}

#[test]
fn a_location_projects_as_a_jscalendar_map_not_a_string() {
    // JSCalendar has no scalar location, and JMAP passes its server's map straight through.
    // A transport that carries one piece of text still has to produce the map, or an
    // override would read differently depending on which door it came in.
    let built = patch(
        OverrideBuilder::new()
            .location_named("Room 2")
            .build()
            .unwrap(),
    );
    let locations = built.get("locations").expect("a locations map");
    let entry = locations
        .as_object()
        .expect("locations is a map")
        .values()
        .next()
        .expect("one location");
    assert_eq!(entry.get("@type").and_then(Value::as_str), Some("Location"));
    assert_eq!(entry.get("name").and_then(Value::as_str), Some("Room 2"));
}

#[test]
fn a_location_carries_its_coordinates_when_it_has_them() {
    let mut location = Location::named("Office");
    location.coordinates = Some("geo:52.37,4.89".to_owned());
    let built = patch(OverrideBuilder::new().location(&location).build().unwrap());
    let entry = built
        .get("locations")
        .and_then(Value::as_object)
        .and_then(|map| map.values().next().cloned())
        .expect("one location");
    assert_eq!(
        entry.get("coordinates").and_then(Value::as_str),
        Some("geo:52.37,4.89")
    );
}

#[test]
fn the_projected_location_id_is_stable_across_builds() {
    // Two syncs of an unchanged occurrence must produce equal patches, or a store that
    // compares them sees a change on every pass.
    let first = patch(
        OverrideBuilder::new()
            .location_named("Room 2")
            .build()
            .unwrap(),
    );
    let second = patch(
        OverrideBuilder::new()
            .location_named("Room 2")
            .build()
            .unwrap(),
    );
    assert_eq!(first.get("locations"), second.get("locations"));
}

#[test]
fn notes_and_title_are_carried_verbatim() {
    let built = patch(
        OverrideBuilder::new()
            .title("Moved to the afternoon")
            .description("Bring the printout")
            .build()
            .unwrap(),
    );
    assert_eq!(
        built.get("title").and_then(Value::as_str),
        Some("Moved to the afternoon")
    );
    assert_eq!(
        built.get("description").and_then(Value::as_str),
        Some("Bring the printout")
    );
}

#[test]
fn a_cancelled_occurrence_says_so_without_leaving_the_series() {
    let built = patch(OverrideBuilder::new().cancelled().build().unwrap());
    assert_eq!(
        built.get("status").and_then(Value::as_str),
        Some("cancelled")
    );
}

#[test]
fn an_untouched_occurrence_builds_an_empty_patch() {
    // Every field is optional, and a field never set is one the occurrence did not change —
    // which is what lets it keep following the master.
    let built = patch(OverrideBuilder::new().build().unwrap());
    assert!(built.get("title").is_none());
    assert!(built.get("start").is_none());
}
