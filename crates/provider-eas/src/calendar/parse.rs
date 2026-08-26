// SPDX-License-Identifier: MPL-2.0

use super::{
    BASE_LOCATION, CAL_ALL_DAY_EVENT, CAL_ATTENDEES, CAL_BUSY_STATUS, CAL_DTSTAMP, CAL_END_TIME,
    CAL_EXCEPTIONS, CAL_LOCATION, CAL_MEETING_STATUS, CAL_ORGANIZER_EMAIL, CAL_ORGANIZER_NAME,
    CAL_RECURRENCE, CAL_REMINDER, CAL_RESPONSE_REQUESTED, CAL_SENSITIVITY, CAL_START_TIME,
    CAL_SUBJECT, CAL_TIMEZONE, CAL_UID, PAGE_CALENDAR,
    attendees::parse_attendees,
    exceptions::parse_exceptions,
    fields::{
        parse_bool_field, parse_calendar_body, parse_datetime_field, parse_enum_field, tag_label,
        text_value_opt,
    },
    location::parse_location_16x,
    model::{CalendarEventProps, TimeZoneBlob},
    recurrence::parse_recurrence,
    timezone::parse_tzi_blob,
};
use crate::wbxml::{
    WbxmlElement, WbxmlError,
    tags::{base, pages},
};

// ============================================================================
// Parse entry
// ============================================================================

/// Parse a Calendar-class `ApplicationData` element into
/// [`CalendarEventProps`].
///
/// `app_data` is the `airsync:ApplicationData` (page 0, 0x1D) child of a
/// Sync Add/Change item whose collection class is `Calendar`. Dispatch is by
/// `(page, token)`: page-4 Calendar tokens plus the two AirSyncBase (page 17)
/// forms the 12.0+/16.x wire uses for Body and Location ([MS-ASWBXML]
/// §2.1.2.1.5 notes 1-2).
///
/// Malformed values → `log::warn!` (element name + offending text) then the
/// field's default; unmodeled tokens → `log::debug!` skip. Never panics.
///
/// The `Err` arm exists for API symmetry with the sync parsers (which return
/// `Result<_, WbxmlError>`); today every malformed shape degrades to a
/// warning + default, so this always returns `Ok`.
///
/// # Errors
///
/// Does not error: every element is either mapped or warn/debug-logged and
/// skipped (the permissive ApplicationData contract). The `Result` keeps the
/// parse-family signature so the Sync dispatcher stays uniform.
pub fn parse_calendar_application_data(
    app_data: &WbxmlElement,
) -> Result<CalendarEventProps, WbxmlError> {
    let mut props = CalendarEventProps::default();
    for child in &app_data.children {
        match (child.page, child.token) {
            (PAGE_CALENDAR, CAL_ALL_DAY_EVENT) => {
                props.all_day_event = parse_bool_field("AllDayEvent", child);
            }
            (PAGE_CALENDAR, CAL_START_TIME) => {
                props.start_time = parse_datetime_field("StartTime", child);
            }
            (PAGE_CALENDAR, CAL_END_TIME) => {
                props.end_time = parse_datetime_field("EndTime", child);
            }
            (PAGE_CALENDAR, CAL_DTSTAMP) => {
                props.dtstamp = parse_datetime_field("DtStamp", child);
            }
            (PAGE_CALENDAR, CAL_SUBJECT) => props.subject = text_value_opt(child),
            // ≤14.1 wire form ([MS-ASCAL] §2.2.2.27): plain-text leaf on
            // the Calendar page.
            (PAGE_CALENDAR, CAL_LOCATION) => props.location = text_value_opt(child),
            // 16.x wire form ([MS-ASWBXML] §2.1.2.1.5 note 2): an
            // AirSyncBase CONTAINER — the value is its DisplayName child
            // (M8-L1).
            (pages::BASE, BASE_LOCATION) => {
                props.location = parse_location_16x("calendar ApplicationData", child);
            }
            // 12.0+ calendar bodies arrive as airsyncbase:Body
            // ([MS-ASWBXML] §2.1.2.1.5 note 1).
            (pages::BASE, base::BODY) => props.body_plain = parse_calendar_body(child),
            (PAGE_CALENDAR, CAL_ORGANIZER_NAME) => {
                props.organizer_name = text_value_opt(child);
            }
            (PAGE_CALENDAR, CAL_ORGANIZER_EMAIL) => {
                props.organizer_email = text_value_opt(child);
            }
            // [MS-ASCAL] §2.2.2.41: 0=Normal 1=Personal 2=Private 3=Confidential.
            (PAGE_CALENDAR, CAL_SENSITIVITY) => {
                props.sensitivity = parse_enum_field("Sensitivity", child, |n| n <= 3);
            }
            // [MS-ASCAL] §2.2.2.9 (v20220429): 0=Free 1=Tentative 2=Busy
            // 3=Out of Office 4=Working elsewhere.
            (PAGE_CALENDAR, CAL_BUSY_STATUS) => {
                props.busy_status = parse_enum_field("BusyStatus", child, |n| n <= 4);
            }
            // [MS-ASCAL] §2.2.2.38: unsignedInt minutes, or an EmptyTag in
            // 16.x meaning "no reminder".
            (PAGE_CALENDAR, CAL_REMINDER) => match text_value_opt(child) {
                Some(raw) => match raw.parse::<u32>() {
                    Ok(minutes) => {
                        props.reminder_set = true;
                        props.reminder_minutes = Some(minutes);
                    }
                    Err(_) => {
                        log::warn!(
                            "calendar ApplicationData: malformed Reminder \"{raw}\"; \
                             expected minutes as unsignedInt, defaulting to no reminder"
                        );
                    }
                },
                None => {
                    log::debug!(
                        "calendar ApplicationData: empty Reminder element (16.x \
                         \"no reminder\" per [MS-ASCAL] §2.2.2.38)"
                    );
                }
            },
            // [MS-ASCAL] §2.2.2.28: wire values {0,1,3,5,7,9,11,13,15}
            // (M/R/C bit flags; 13 = "Same as 5" — cancelled, organizer).
            (PAGE_CALENDAR, CAL_MEETING_STATUS) => {
                props.meeting_status = parse_enum_field("MeetingStatus", child, |n| {
                    matches!(n, 0 | 1 | 3 | 5 | 7 | 9 | 11 | 13 | 15)
                });
            }
            (PAGE_CALENDAR, CAL_RESPONSE_REQUESTED) => {
                props.response_requested = parse_bool_field("ResponseRequested", child);
            }
            // [MS-ASCAL] §2.2.2.46 (12.0+): the item's GlobalObjectId
            // identity string, kept verbatim (≤300 chars per spec — length
            // not enforced on downsync, fidelity first). An empty value (a
            // gateway clearing it) stays None: never an invented join key.
            (PAGE_CALENDAR, CAL_UID) => {
                props.uid = text_value_opt(child).filter(|s| !s.is_empty());
            }
            // [MS-ASDTYPE] §2.7.6: base64 TZI blob in a string element; the
            // raw string is always kept, the decode degrades to None.
            (PAGE_CALENDAR, CAL_TIMEZONE) => match text_value_opt(child) {
                Some(raw) if !raw.is_empty() => {
                    let parsed = parse_tzi_blob(&raw);
                    props.time_zone = Some(TimeZoneBlob {
                        raw_base64: Some(raw),
                        parsed,
                    });
                }
                Some(_) => {
                    log::warn!("calendar ApplicationData: empty Timezone text; ignoring");
                }
                None => {
                    log::warn!(
                        "calendar ApplicationData: Timezone element without a text \
                         value; ignoring"
                    );
                }
            },
            // Task-3 containers ([MS-ASCAL] §2.2.2.37/§2.2.2.22/§2.2.2.4).
            (PAGE_CALENDAR, CAL_RECURRENCE) => {
                props.recurrence = Some(parse_recurrence(child));
            }
            (PAGE_CALENDAR, CAL_EXCEPTIONS) => {
                props.exceptions = parse_exceptions(child);
            }
            (PAGE_CALENDAR, CAL_ATTENDEES) => {
                props.attendees = parse_attendees(child);
            }
            _ => {
                log::debug!(
                    "calendar ApplicationData: skipping unmodeled element {} \
                     (page {} token 0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    Ok(props)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{
            CalendarAttendee, CalendarException, CalendarRecurrence, TziTimeZone,
            tests::{TZI_FLAT_UTC8, fixture_full_app_data},
        },
        commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    };

    /// Full item: every field populated, Task-3 containers included.
    /// Golden assertion over the whole struct.
    #[test]
    fn parse_full_core_item() {
        let props = parse_calendar_application_data(&fixture_full_app_data())
            .expect("parse must not fail on a well-formed item");
        assert_eq!(
            props,
            CalendarEventProps {
                all_day_event: false,
                start_time: Some("20260818T090000Z".to_string()),
                end_time: Some("20260818T100000Z".to_string()),
                dtstamp: Some("20260815T120000Z".to_string()),
                subject: Some("Weekly Sync".to_string()),
                location: Some("Room 42".to_string()),
                body_plain: Some("Agenda: sync status".to_string()),
                organizer_name: Some("Felix Zhou".to_string()),
                organizer_email: Some("felixzhou@kylins.local".to_string()),
                sensitivity: Some(2),
                busy_status: Some(2),
                reminder_set: true,
                reminder_minutes: Some(15),
                meeting_status: Some(1),
                response_requested: true,
                uid: None,
                time_zone: Some(TimeZoneBlob {
                    raw_base64: Some(TZI_FLAT_UTC8.to_string()),
                    parsed: Some(TziTimeZone {
                        base_bias_minutes: -480,
                        standard: None,
                        daylight: None,
                    }),
                }),
                recurrence: Some(CalendarRecurrence {
                    recurrence_type: 1,
                    interval: Some(1),
                    day_of_week: Some(62),
                    until: Some("20261225T090000Z".to_string()),
                    no_end: false,
                    ..Default::default()
                }),
                exceptions: vec![
                    CalendarException {
                        deleted: true,
                        exception_start_time: Some("20260825T090000Z".to_string()),
                        ..Default::default()
                    },
                    CalendarException {
                        deleted: false,
                        exception_start_time: Some("20260901T090000Z".to_string()),
                        start_time: Some("20260901T100000Z".to_string()),
                        end_time: Some("20260901T110000Z".to_string()),
                        subject: Some("Moved".to_string()),
                        location: Some("Room 7".to_string()),
                        body_plain: None,
                        // The fixture carries AllDayEvent "0" → Some(false);
                        // the deleted marker above omits it → None.
                        all_day_event: Some(false),
                    },
                ],
                attendees: vec![
                    CalendarAttendee {
                        name: Some("Bob".to_string()),
                        email: "bob@example.com".to_string(),
                        status: Some(3),
                    },
                    CalendarAttendee {
                        name: Some("Carol".to_string()),
                        email: "carol@example.com".to_string(),
                        status: None,
                    },
                ],
            }
        );
    }

    /// All-day item ([MS-ASCAL] §2.2.2.1: midnight → next midnight).
    #[test]
    fn parse_all_day_item() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Company Holiday"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "1"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "20260820T000000Z"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260821T000000Z"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_BUSY_STATUS, "0"),
            ],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert!(props.all_day_event);
        assert_eq!(props.subject.as_deref(), Some("Company Holiday"));
        assert_eq!(props.start_time.as_deref(), Some("20260820T000000Z"));
        assert_eq!(props.end_time.as_deref(), Some("20260821T000000Z"));
        assert_eq!(props.busy_status, Some(0));
        // Everything absent stays at its default.
        assert_eq!(props.location, None);
        assert_eq!(props.body_plain, None);
        assert_eq!(props.organizer_name, None);
        assert_eq!(props.organizer_email, None);
        assert!(!props.reminder_set);
        assert_eq!(props.reminder_minutes, None);
        assert_eq!(props.meeting_status, None);
        assert!(!props.response_requested);
        assert_eq!(props.time_zone, None);
    }

    /// Absent optionals: an empty ApplicationData yields all defaults —
    /// no panic, no phantom Some values.
    #[test]
    fn parse_absent_optionals_are_defaults() {
        let app_data = WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, vec![]);
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props, CalendarEventProps::default());
    }

    /// Malformed StartTime: warn + None, never panic — and the rest of the
    /// item still parses (a bad timestamp must not poison sibling fields).
    #[test]
    fn parse_malformed_start_time_warns_and_defaults_none() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Bad clock"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "not-a-date"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260818T100000Z"),
            ],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.start_time, None, "malformed StartTime must be None");
        assert_eq!(props.subject.as_deref(), Some("Bad clock"));
        assert_eq!(props.end_time.as_deref(), Some("20260818T100000Z"));
    }

    /// Malformed EndTime / empty DtStamp also degrade to None without panic.
    #[test]
    fn parse_malformed_end_time_and_dtstamp_default_none() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "garbage"),
                WbxmlElement::empty(PAGE_CALENDAR, CAL_DTSTAMP),
            ],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.end_time, None);
        assert_eq!(props.dtstamp, None);
    }

    /// Reminder variants: numeric → set + minutes; EmptyTag (16.x "no
    /// reminder", [MS-ASCAL] §2.2.2.38) → not set.
    #[test]
    fn parse_reminder_variants() {
        let empty = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::empty(PAGE_CALENDAR, CAL_REMINDER)],
        );
        let props = parse_calendar_application_data(&empty).expect("parse ok");
        assert!(!props.reminder_set);
        assert_eq!(props.reminder_minutes, None);

        let numeric = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(PAGE_CALENDAR, CAL_REMINDER, "0")],
        );
        let props = parse_calendar_application_data(&numeric).expect("parse ok");
        assert!(props.reminder_set, "Reminder=0 is a reminder AT start");
        assert_eq!(props.reminder_minutes, Some(0));
    }

    /// Malformed enums: out-of-range Sensitivity (>3), non-numeric
    /// BusyStatus, and off-set MeetingStatus all degrade to None with a
    /// warning — never panic, never a bogus value.
    #[test]
    fn parse_malformed_enums_default_to_none() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_SENSITIVITY, "7"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_BUSY_STATUS, "abc"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_MEETING_STATUS, "2"),
            ],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.sensitivity, None);
        assert_eq!(props.busy_status, None);
        assert_eq!(props.meeting_status, None);
    }

    /// MeetingStatus 13 is a valid wire value per [MS-ASCAL] §2.2.2.28
    /// v20220429 ("Same as 5" — cancelled, user is the organizer); it was
    /// missing from the original allowlist and surfaced during the M8
    /// Task-6 spec verification (the ICS STATUS:CANCELLED mapping depends
    /// on it reaching the converter).
    #[test]
    fn parse_meeting_status_13_cancelled_organizer_is_valid() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(PAGE_CALENDAR, CAL_MEETING_STATUS, "13")],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.meeting_status, Some(13));
    }

    /// Malformed boolean (AllDayEvent "2") defaults to false with a warning.
    #[test]
    fn parse_malformed_bool_defaults_false() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "2")],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert!(!props.all_day_event);
    }

    /// Unmodeled tokens (Categories 0x0E; CalendarType 0x37; unknown
    /// page/token garbage) are skipped without panic and do not disturb the
    /// modeled fields. (UID 0x28 used to live here — it is modeled since M8
    /// follow-up #4, see `parse_top_level_uid`.)
    #[test]
    fn parse_unmodeled_tokens_are_skipped() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::container(
                    PAGE_CALENDAR,
                    0x0E, // Categories
                    vec![WbxmlElement::text(PAGE_CALENDAR, 0x0F, "Work")],
                ),
                WbxmlElement::text(PAGE_CALENDAR, 0x37, "1"), // CalendarType
                WbxmlElement::text(0xFE, 0x7F, "garbage"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Real Subject"),
            ],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.subject.as_deref(), Some("Real Subject"));
        assert_eq!(
            props,
            CalendarEventProps {
                subject: Some("Real Subject".to_string()),
                ..Default::default()
            }
        );
    }

    // ====================================================================
    // M8 follow-up #4 — top-level UID ([MS-ASCAL] §2.2.2.46)
    // ====================================================================

    /// The calendar:UID element (4, 0x28) parses verbatim into
    /// `CalendarEventProps.uid` — the exact-key invite↔event correlation
    /// identity ([MS-ASEMAIL] §3.1.4.7; [MS-ASWBXML] §2.1.2.1.4 note 4).
    #[test]
    fn parse_top_level_uid() {
        const GO: &str = "040000008200E00074C5B7101A82E00800000000E040C9C12685C401";
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Weekly Sync"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_UID, GO),
            ],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.uid.as_deref(), Some(GO), "UID must parse verbatim");
        assert_eq!(props.subject.as_deref(), Some("Weekly Sync"));
    }

    /// An empty UID element (a gateway clearing the value) is a warn + None,
    /// never an invented key.
    #[test]
    fn parse_empty_uid_is_none() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::empty(PAGE_CALENDAR, CAL_UID)],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.uid, None);
    }

    /// Empty Timezone element/text degrades to None (no phantom blob).
    #[test]
    fn parse_empty_timezone_is_none() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::empty(PAGE_CALENDAR, CAL_TIMEZONE)],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.time_zone, None);
    }
}
