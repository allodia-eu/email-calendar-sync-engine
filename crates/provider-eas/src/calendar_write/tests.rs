// SPDX-License-Identifier: MPL-2.0
// Builder + validation tests, with the shared write fixtures.

use super::*;
use crate::{
    calendar::{
        CAL_ALL_DAY_EVENT, CAL_ATTENDEE, CAL_ATTENDEE_EMAIL, CAL_ATTENDEE_NAME, CAL_ATTENDEES,
        CAL_BUSY_STATUS, CAL_END_TIME, CAL_LOCATION, CAL_RECURRENCE, CAL_RECURRENCE_DAY_OF_MONTH,
        CAL_RECURRENCE_DAY_OF_WEEK, CAL_RECURRENCE_INTERVAL, CAL_RECURRENCE_MONTH_OF_YEAR,
        CAL_RECURRENCE_OCCURRENCES, CAL_RECURRENCE_TYPE, CAL_RECURRENCE_UNTIL, CAL_REMINDER,
        CAL_SENSITIVITY, CAL_START_TIME, CAL_SUBJECT, CAL_TIMEZONE, CalendarAttendee,
        CalendarRecurrence, PAGE_CALENDAR, TimeZoneBlob, TziTimeZone,
        parse_calendar_application_data,
    },
    commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    wbxml::{
        WbxmlElement, WbxmlValue, deserialize_to_tree, serialize_tree,
        tags::{base, pages},
    },
};

/// Minimal valid write: only the four required fields (UTC+8 flat zone).
fn minimal_write() -> CalendarEventWrite {
    CalendarEventWrite {
        start_time: "20260818T090000Z".to_string(),
        end_time: "20260818T100000Z".to_string(),
        all_day_event: false,
        time_zone_base64: build_fixed_offset_tzi_base64(480),
        ..Default::default()
    }
}

/// Fully-populated write covering every emitted element.
fn full_write() -> CalendarEventWrite {
    CalendarEventWrite {
        subject: Some("Weekly Sync".to_string()),
        location: Some("Room 42".to_string()),
        body_plain: Some("Agenda: sync status".to_string()),
        organizer_email: Some("felixzhou@kylins.local".to_string()),
        organizer_name: Some("Felix Zhou".to_string()),
        sensitivity: Some(2),
        busy_status: Some(2),
        reminder_minutes: Some(15),
        attendees: vec![
            // status Some(3) must NOT be written (server-owned).
            CalendarAttendee {
                name: Some("Bob".to_string()),
                email: "bob@example.com".to_string(),
                status: Some(3),
            },
            // name None ⇒ Name child omitted.
            CalendarAttendee {
                name: None,
                email: "carol@example.com".to_string(),
                status: None,
            },
        ],
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 1,
            interval: Some(1),
            day_of_week: Some(62),
            until: Some("20261225T090000Z".to_string()),
            no_end: false,
            ..Default::default()
        }),
        ..minimal_write()
    }
}

/// (page, token) sequence of an element's children.
fn tag_seq(elem: &WbxmlElement) -> Vec<(u8, u8)> {
    elem.children.iter().map(|c| (c.page, c.token)).collect()
}

/// Text of a leaf element (panics with the tag id when it is not text).
fn text_of(elem: &WbxmlElement) -> &str {
    match &elem.value {
        WbxmlValue::Text(s) => s,
        _ => panic!(
            "expected a text value on (page {}, token 0x{:02X})",
            elem.page, elem.token
        ),
    }
}

// ====================================================================
// TZI synthesis (design D6)
// ====================================================================

/// Fully-populated write: the EXACT canonical (page, token) child
/// sequence, nested containers included, plus the always-emitted leaf
/// values. Server-managed elements (UID/DtStamp/MeetingStatus/
/// ResponseRequested/AttendeeStatus) are absent because the sequence
/// match is exhaustive.
#[test]
fn full_write_emits_canonical_token_sequence() {
    let w = full_write();
    let app_data = build_calendar_application_data(&w, "16.1");
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(
        tag_seq(&app_data),
        vec![
            (PAGE_CALENDAR, CAL_TIMEZONE),
            (PAGE_CALENDAR, CAL_ALL_DAY_EVENT),
            (PAGE_CALENDAR, CAL_START_TIME),
            (PAGE_CALENDAR, CAL_END_TIME),
            (PAGE_CALENDAR, CAL_SUBJECT),
            // 16.x Location: the airsyncbase CONTAINER (page 17, 0x20),
            // not the legacy calendar-page leaf — [MS-ASWBXML]
            // §2.1.2.1.5 note 2; live evidence 2026-08-22 (a 16.1
            // server answers the legacy leaf with per-item Status 6).
            (pages::BASE, base::LOCATION),
            (pages::BASE, base::BODY),
            // OrganizerEmail/OrganizerName are server-managed on write
            // (Status 6 on live 16.1 — probe 2026-08-22): absent here.
            (PAGE_CALENDAR, CAL_SENSITIVITY),
            (PAGE_CALENDAR, CAL_BUSY_STATUS),
            (PAGE_CALENDAR, CAL_REMINDER),
            (PAGE_CALENDAR, CAL_ATTENDEES),
            (PAGE_CALENDAR, CAL_RECURRENCE),
        ]
    );
    // Always-emitted leaf values.
    assert_eq!(text_of(&app_data.children[0]), w.time_zone_base64);
    assert_eq!(text_of(&app_data.children[1]), "0");
    assert_eq!(text_of(&app_data.children[2]), "20260818T090000Z");
    assert_eq!(text_of(&app_data.children[3]), "20260818T100000Z");
    // Option-gated leaf values.
    assert_eq!(text_of(&app_data.children[4]), "Weekly Sync");
    // Location on 16.x = container whose DisplayName carries the value
    // (the M8-L1 downsync shape).
    let location = &app_data.children[5];
    assert_eq!(
        (location.page, location.token),
        (pages::BASE, base::LOCATION)
    );
    assert_eq!(tag_seq(location), vec![(pages::BASE, base::DISPLAY_NAME)]);
    assert_eq!(text_of(&location.children[0]), "Room 42");
    assert_eq!(text_of(&app_data.children[7]), "2");
    assert_eq!(text_of(&app_data.children[8]), "2");
    assert_eq!(text_of(&app_data.children[9]), "15");
    // Body: page-17 container, Type "1" (PlainText) + Data.
    let body = &app_data.children[6];
    assert_eq!(
        tag_seq(body),
        vec![(pages::BASE, base::TYPE), (pages::BASE, base::DATA)]
    );
    assert_eq!(text_of(&body.children[0]), "1");
    assert_eq!(text_of(&body.children[1]), "Agenda: sync status");
    // Attendees: Email first, Name only when Some, AttendeeStatus never.
    let attendees = &app_data.children[10];
    assert_eq!(
        tag_seq(attendees),
        vec![(PAGE_CALENDAR, CAL_ATTENDEE), (PAGE_CALENDAR, CAL_ATTENDEE)]
    );
    assert_eq!(
        tag_seq(&attendees.children[0]),
        vec![
            (PAGE_CALENDAR, CAL_ATTENDEE_EMAIL),
            (PAGE_CALENDAR, CAL_ATTENDEE_NAME),
        ]
    );
    assert_eq!(
        text_of(&attendees.children[0].children[0]),
        "bob@example.com"
    );
    assert_eq!(text_of(&attendees.children[0].children[1]), "Bob");
    assert_eq!(
        tag_seq(&attendees.children[1]),
        vec![(PAGE_CALENDAR, CAL_ATTENDEE_EMAIL)],
        "nameless attendee emits Email only"
    );
    // Recurrence: Type, Interval, DayOfWeek, Until — in that order.
    let recurrence = &app_data.children[11];
    assert_eq!(
        tag_seq(recurrence),
        vec![
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE),
            (PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL),
            (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK),
            (PAGE_CALENDAR, CAL_RECURRENCE_UNTIL),
        ]
    );
    assert_eq!(text_of(&recurrence.children[0]), "1");
    assert_eq!(text_of(&recurrence.children[3]), "20261225T090000Z");
}

/// Minimal write: ONLY the four required elements — no Attendees
/// container, no Option leaves, no Recurrence.
#[test]
fn minimal_write_emits_only_required_elements() {
    let app_data = build_calendar_application_data(&minimal_write(), "16.1");
    assert_eq!(
        tag_seq(&app_data),
        vec![
            (PAGE_CALENDAR, CAL_TIMEZONE),
            (PAGE_CALENDAR, CAL_ALL_DAY_EVENT),
            (PAGE_CALENDAR, CAL_START_TIME),
            (PAGE_CALENDAR, CAL_END_TIME),
        ]
    );
}

/// All-day write ([MS-ASCAL] §2.2.2.1): AllDayEvent "1" with UTC
/// midnight-to-midnight times.
#[test]
fn all_day_write_emits_one_and_utc_midnight() {
    let w = CalendarEventWrite {
        start_time: "20260820T000000Z".to_string(),
        end_time: "20260821T000000Z".to_string(),
        all_day_event: true,
        time_zone_base64: build_fixed_offset_tzi_base64(480),
        ..Default::default()
    };
    let app_data = build_calendar_application_data(&w, "16.1");
    assert_eq!(text_of(&app_data.children[1]), "1");
    assert_eq!(text_of(&app_data.children[2]), "20260820T000000Z");
    assert_eq!(text_of(&app_data.children[3]), "20260821T000000Z");
}

/// ≤14.1 servers keep the calendar-page Location leaf (the pre-16
/// generation of [MS-ASWBXML] §2.1.2.1.5 note 2); the negotiated
/// version picks the wire generation.
#[test]
fn legacy_version_emits_page4_location_leaf() {
    let w = CalendarEventWrite {
        location: Some("Room 42".to_string()),
        ..full_write()
    };
    let app_data = build_calendar_application_data(&w, "14.1");
    // Subject first, then the LEGACY leaf directly on page 4 (0x17) —
    // no page-17 container in the sequence at all.
    assert_eq!(
        tag_seq(&app_data)[4],
        (PAGE_CALENDAR, CAL_SUBJECT),
        "subject stays before location"
    );
    assert_eq!(
        tag_seq(&app_data)[5],
        (PAGE_CALENDAR, CAL_LOCATION),
        "≤14.1 keeps the calendar-page Location leaf"
    );
    assert!(
        !tag_seq(&app_data).contains(&(pages::BASE, base::LOCATION)),
        "no airsyncbase container on the legacy wire"
    );
    assert_eq!(text_of(&app_data.children[5]), "Room 42");
}

/// Recurrence end-condition variants: Until-only, Occurrences-only, and
/// neither (no_end is derived — NEVER a wire element). Both set is a
/// struct-invariant violation: Until wins (Occasions suppressed) so the
/// wire never carries the mutually exclusive pair ([MS-ASCAL]
/// §2.2.2.47).
#[test]
fn recurrence_until_xor_occurrences_variants() {
    let build = |rec: Option<CalendarRecurrence>| {
        let w = CalendarEventWrite {
            recurrence: rec,
            ..minimal_write()
        };
        build_calendar_application_data(&w, "16.1").children[4].clone()
    };

    let until_only = build(Some(CalendarRecurrence {
        recurrence_type: 1,
        interval: Some(1),
        until: Some("20261231T235959Z".to_string()),
        ..Default::default()
    }));
    assert_eq!(
        tag_seq(&until_only),
        vec![
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE),
            (PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL),
            (PAGE_CALENDAR, CAL_RECURRENCE_UNTIL),
        ]
    );

    let occurrences_only = build(Some(CalendarRecurrence {
        recurrence_type: 0,
        interval: Some(2),
        occurrences: Some(10),
        no_end: false,
        ..Default::default()
    }));
    assert_eq!(
        tag_seq(&occurrences_only),
        vec![
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE),
            (PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL),
            (PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES),
        ]
    );
    assert_eq!(text_of(&occurrences_only.children[2]), "10");

    // Neither ⇒ no end element at all (no_end is derived, not a token).
    let no_end = build(Some(CalendarRecurrence {
        recurrence_type: 5,
        day_of_month: Some(1),
        month_of_year: Some(6),
        ..Default::default()
    }));
    assert_eq!(
        tag_seq(&no_end),
        vec![
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE),
            (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH),
            (PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR),
        ]
    );

    // Both set: Until wins, Occurrences suppressed.
    let both = build(Some(CalendarRecurrence {
        recurrence_type: 0,
        until: Some("20261231T235959Z".to_string()),
        occurrences: Some(5),
        ..Default::default()
    }));
    assert_eq!(
        tag_seq(&both),
        vec![
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE),
            (PAGE_CALENDAR, CAL_RECURRENCE_UNTIL),
        ]
    );
}

// ====================================================================
// Write → parse round-trip (the strongest golden: the downsync parser
// must recover exactly what the write model carried)
// ====================================================================

/// Fully-populated event: every carried field survives — datetimes,
/// subject/location/body, organizer, enums, reminder, TZI (raw + parsed
/// −480 flat), attendees (minus the server-owned status), recurrence
/// (no_end re-derived false). Server-managed fields stay at defaults.
#[test]
fn write_parse_round_trip_full_event() {
    let w = full_write();
    let props = parse_calendar_application_data(&build_calendar_application_data(&w, "16.1"))
        .expect("parse ok");
    assert!(!props.all_day_event);
    assert_eq!(props.start_time.as_deref(), Some("20260818T090000Z"));
    assert_eq!(props.end_time.as_deref(), Some("20260818T100000Z"));
    assert_eq!(props.subject.as_deref(), Some("Weekly Sync"));
    assert_eq!(props.location.as_deref(), Some("Room 42"));
    assert_eq!(props.body_plain.as_deref(), Some("Agenda: sync status"));
    // Organizer fields are server-managed on write (Status 6 on live
    // 16.1 — probe 2026-08-22): never emitted, so they never round-trip.
    assert_eq!(props.organizer_email, None);
    assert_eq!(props.organizer_name, None);
    assert_eq!(props.sensitivity, Some(2));
    assert_eq!(props.busy_status, Some(2));
    assert!(props.reminder_set);
    assert_eq!(props.reminder_minutes, Some(15));
    assert_eq!(
        props.time_zone,
        Some(TimeZoneBlob {
            raw_base64: Some(w.time_zone_base64.clone()),
            parsed: Some(TziTimeZone {
                base_bias_minutes: -480,
                standard: None,
                daylight: None,
            }),
        })
    );
    assert_eq!(
        props.recurrence,
        Some(CalendarRecurrence {
            recurrence_type: 1,
            interval: Some(1),
            day_of_week: Some(62),
            until: Some("20261225T090000Z".to_string()),
            no_end: false,
            ..Default::default()
        })
    );
    assert_eq!(props.attendees.len(), 2);
    assert_eq!(props.attendees[0].name.as_deref(), Some("Bob"));
    assert_eq!(props.attendees[0].email, "bob@example.com");
    assert_eq!(
        props.attendees[0].status, None,
        "AttendeeStatus is server-owned on write — must not round-trip"
    );
    assert_eq!(props.attendees[1].name, None);
    assert_eq!(props.attendees[1].email, "carol@example.com");
    // Server-managed fields stay at their defaults.
    assert_eq!(props.dtstamp, None);
    assert_eq!(props.meeting_status, None);
    assert_eq!(props.uid, None);
    assert!(!props.response_requested);
    assert!(props.exceptions.is_empty());
}

/// Minimal all-day event round-trips with UTC midnights intact.
#[test]
fn write_parse_round_trip_all_day_minimal() {
    let w = CalendarEventWrite {
        start_time: "20260820T000000Z".to_string(),
        end_time: "20260821T000000Z".to_string(),
        all_day_event: true,
        time_zone_base64: build_fixed_offset_tzi_base64(480),
        ..Default::default()
    };
    let props = parse_calendar_application_data(&build_calendar_application_data(&w, "16.1"))
        .expect("parse ok");
    assert!(props.all_day_event);
    assert_eq!(props.start_time.as_deref(), Some("20260820T000000Z"));
    assert_eq!(props.end_time.as_deref(), Some("20260821T000000Z"));
    assert_eq!(
        props
            .time_zone
            .as_ref()
            .and_then(|t| t.parsed.clone())
            .map(|t| t.base_bias_minutes),
        Some(-480)
    );
    assert_eq!(props.attendees, Vec::new());
    assert_eq!(props.recurrence, None);
}

// ====================================================================
// WBXML byte round-trip
// ====================================================================

/// serialize_tree → deserialize_to_tree must reproduce the element
/// exactly — page switching (0 → 4 → 17) included.
#[test]
fn wbxml_serialize_deserialize_round_trip() {
    let elem = build_calendar_application_data(&full_write(), "16.1");
    let bytes = serialize_tree(&elem).expect("serialize");
    let back = deserialize_to_tree(&bytes).expect("deserialize");
    assert_eq!(back, elem);
}

// ====================================================================
// validate
// ====================================================================

/// Accept path: the full write (sensitivity 2 ≤ 3, busy 2 ≤ 4) and the
/// enum upper bounds (3 / 4) validate.
#[test]
fn validate_accepts_valid_event_and_enum_bounds() {
    assert_eq!(full_write().validate(), Ok(()));
    let mut w = minimal_write();
    w.sensitivity = Some(3);
    w.busy_status = Some(4);
    assert_eq!(w.validate(), Ok(()));
}

/// Reject paths: malformed datetimes (non-datetime start; datetime-shaped
/// end without a zone), empty TZI, out-of-range enums.
#[test]
fn validate_rejects_bad_shapes() {
    let mut w = minimal_write();
    w.start_time = "not-a-date".to_string();
    assert!(matches!(
        w.validate(),
        Err(CalendarWriteError::InvalidStartTime(_))
    ));

    let mut w = minimal_write();
    w.end_time = "2026-08-18T09:00:00".to_string(); // no zone designator
    assert!(matches!(
        w.validate(),
        Err(CalendarWriteError::InvalidEndTime(_))
    ));

    let mut w = minimal_write();
    w.time_zone_base64 = String::new();
    assert!(matches!(
        w.validate(),
        Err(CalendarWriteError::EmptyTimeZone)
    ));

    let mut w = minimal_write();
    w.sensitivity = Some(4);
    assert_eq!(
        w.validate(),
        Err(CalendarWriteError::SensitivityOutOfRange(4))
    );

    let mut w = minimal_write();
    w.busy_status = Some(5);
    assert_eq!(
        w.validate(),
        Err(CalendarWriteError::BusyStatusOutOfRange(5))
    );
}
