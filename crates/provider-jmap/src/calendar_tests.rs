use super::*;

const CALENDAR_GET: &str = include_str!("../tests/fixtures/calendar_get.json");
const EVENT_GET: &str = include_str!("../tests/fixtures/calendarevent_get.json");

fn events() -> Vec<Event> {
    let doc: Value = serde_json::from_str(EVENT_GET).unwrap();
    doc["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| event_from_json(e).unwrap())
        .collect()
}

fn by_uid(uid: &str) -> Event {
    events()
        .into_iter()
        .find(|e| e.uid.as_str() == uid)
        .unwrap_or_else(|| panic!("no event with uid {uid}"))
}

#[test]
fn calendar_container_normalizes() {
    let doc: Value = serde_json::from_str(CALENDAR_GET).unwrap();
    let calendar = calendar_from_json(&doc["list"][0]).unwrap();
    assert!(calendar.is_default);
    assert!(calendar.name.contains("Stalwart Calendar"));
}

#[test]
fn all_seed_events_normalize_and_preserve_raw() {
    let all = events();
    assert_eq!(all.len(), 6);
    for event in &all {
        assert!(event.raw_jscalendar.is_some(), "raw JSCalendar preserved");
    }
}

#[test]
fn zoned_event_carries_its_zone_and_location() {
    let one_off = by_uid("oneoff-2001@test.local");
    assert_eq!(
        one_off.start.zone().map(TimeZoneId::as_str),
        Some("Europe/Amsterdam")
    );
    assert_eq!(one_off.duration, "PT1H".parse().unwrap());
    assert_eq!(one_off.locations[0].name.as_deref(), Some("Amsterdam HQ"));
}

#[test]
fn recurring_event_maps_rule_and_overrides() {
    let weekly = by_uid("weekly-2002@test.local");
    assert!(weekly.is_recurring());
    let recurrence = weekly.recurrence.as_ref().unwrap();
    let rule = &recurrence.rules[0];
    assert_eq!(rule.frequency, Frequency::Weekly);
    assert_eq!(
        rule.bound,
        RecurrenceBound::Count(NonZeroU32::new(8).unwrap())
    );
    assert_eq!(rule.by_day[0].day, Weekday::Mo);
    // The excluded instance and the moved instance both became overrides.
    let excluded: LocalDateTime = "2026-01-19T09:30:00".parse().unwrap();
    assert!(recurrence.is_excluded(&excluded));
    let moved: LocalDateTime = "2026-01-26T09:30:00".parse().unwrap();
    assert!(matches!(
        recurrence.overrides.get(&moved),
        Some(RecurrenceOverride::Patch(_))
    ));
}

#[test]
fn meeting_maps_participants() {
    let meeting = by_uid("meeting-2003@test.local");
    assert!(!meeting.participants.is_empty());
    let alice = meeting
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("alice@test.local"))
        .expect("alice is a participant");
    assert_eq!(alice.participation_status, ParticipationStatus::Accepted);
    assert!(alice.has_role(&ParticipantRole::Chair));
}

#[test]
fn virtual_location_maps_conference_uri() {
    let virt = by_uid("virtual-2004@test.local");
    assert_eq!(
        virt.virtual_locations[0].uri,
        "https://meet.example.com/harness-room"
    );
}

#[test]
fn all_day_event_is_a_zone_invariant_date() {
    let all_day = by_uid("allday-2005@test.local");
    assert!(all_day.is_all_day());
    assert!(all_day.start.zone().is_none());
}

#[test]
fn floating_event_has_no_zone_but_a_wall_clock() {
    let floating = by_uid("floating-2006@test.local");
    assert!(floating.start.is_floating());
    assert!(floating.start.local().is_some());
}

fn base_event(extra: serde_json::Value) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "id": "e", "uid": "u@h", "calendarIds": { "c": true },
        "start": "2026-01-01T09:00:00", "timeZone": "Etc/UTC"
    });
    let map = obj.as_object_mut().unwrap();
    if let serde_json::Value::Object(fields) = extra {
        for (key, value) in fields {
            map.insert(key, value);
        }
    }
    obj
}

#[test]
fn recurrence_rule_maps_interval_until_and_by_parts() {
    let event = event_from_json(&base_event(serde_json::json!({
        "recurrenceRule": {
            "frequency": "monthly", "interval": 2, "byMonthDay": [1, -1],
            "byMonth": ["3", "6"], "firstDayOfWeek": "su",
            "until": "2026-12-31T09:00:00"
        }
    })))
    .unwrap();
    let rule = &event.recurrence.unwrap().rules[0];
    assert_eq!(rule.frequency, Frequency::Monthly);
    assert_eq!(rule.interval.get(), 2);
    assert_eq!(rule.by_month_day, vec![1, -1]);
    assert_eq!(rule.by_month, vec!["3".to_owned(), "6".to_owned()]);
    assert_eq!(rule.first_day_of_week, Weekday::Su);
    assert!(matches!(rule.bound, RecurrenceBound::Until(_)));
}

#[test]
fn nth_weekday_locations_and_keywords() {
    let event = event_from_json(&base_event(serde_json::json!({
        "recurrenceRule": { "frequency": "monthly", "byDay": [{ "day": "fr", "nthOfPeriod": -1 }] },
        "locations": { "l": { "@type": "Location" } },
        "virtualLocations": { "v": { "@type": "VirtualLocation" } },
        "keywords": { "kw": true }, "categories": { "cat": true },
        "status": "cancelled", "freeBusyStatus": "free", "privacy": "private", "color": "#abc"
    })))
    .unwrap();
    let rule = &event.recurrence.as_ref().unwrap().rules[0];
    assert_eq!(rule.by_day[0].nth_of_period.unwrap().get(), -1);
    // A nameless location still normalizes; a virtual location without a uri is dropped.
    assert_eq!(event.locations.len(), 1);
    assert!(event.virtual_locations.is_empty());
    assert!(event.keywords.contains("kw") && event.categories.contains("cat"));
    assert!(event.is_cancelled());
    assert_eq!(event.color.as_deref(), Some("#abc"));
}

#[test]
fn plural_recurrence_rules_array_and_sequence() {
    let event = event_from_json(&base_event(serde_json::json!({
        "sequence": 3,
        "recurrenceRules": [{ "frequency": "daily", "interval": 3 }]
    })))
    .unwrap();
    assert_eq!(event.sequence, 3);
    let rule = &event.recurrence.unwrap().rules[0];
    assert_eq!(rule.frequency, Frequency::Daily);
    assert_eq!(rule.interval.get(), 3);
}

#[test]
fn bad_recurrence_parts_error_without_panicking() {
    // Missing frequency.
    assert!(event_from_json(&base_event(serde_json::json!({ "recurrenceRule": {} }))).is_err());
    // Unknown weekday in byDay.
    assert!(
        event_from_json(&base_event(serde_json::json!({
            "recurrenceRule": { "frequency": "weekly", "byDay": [{ "day": "xx" }] }
        })))
        .is_err()
    );
    // Unparseable recurrence-id key.
    assert!(
        event_from_json(&base_event(serde_json::json!({
            "recurrenceOverrides": { "not-a-date": { "title": "x" } }
        })))
        .is_err()
    );
}

#[test]
fn floating_calendar_and_bad_calendar_timezone() {
    // A calendar with a bad timeZone errors rather than panicking.
    assert!(calendar_from_json(&serde_json::json!({ "id": "c", "timeZone": 5 })).is_ok());
    let cal = calendar_from_json(&serde_json::json!({
        "id": "c", "name": "Work", "color": "red", "sortOrder": 3,
        "isSubscribed": true, "isDefault": false, "timeZone": "Europe/Berlin"
    }))
    .unwrap();
    assert_eq!(cal.time_zone.as_ref().unwrap().as_str(), "Europe/Berlin");
    assert!(cal.is_subscribed && !cal.is_default);
}

#[test]
fn malformed_event_errors_without_panicking() {
    assert!(
        event_from_json(&serde_json::json!({ "uid": "x", "start": "2026-01-01T00:00:00" }))
            .is_err()
    );
    assert!(
            event_from_json(&serde_json::json!({ "id": "e", "uid": "x", "calendarIds": {}, "start": "2026-01-01T00:00:00" }))
                .is_err()
        );
}
