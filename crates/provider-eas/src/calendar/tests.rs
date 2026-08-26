// SPDX-License-Identifier: MPL-2.0
//! Shared golden fixtures — `pub(crate)` so the class-aware Sync seam
//! tests in `commands/sync/tests.rs` and the `calendar_write` tests reuse the SAME
//! blobs and wire tree instead of transcribing copies.

use super::*;
use crate::{
    commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    wbxml::{WbxmlElement, tags::pages},
};

/// Cross-check every constant against its [MS-ASWBXML] §2.1.2.1.5 /
/// §2.1.2.1.18 value AND against the `code_pages.rs` registration
/// (tag_name resolution), so a drifted constant fails loudly.
#[test]
fn calendar_token_constants_match_spec() {
    assert_eq!(PAGE_CALENDAR, 4);
    assert_eq!(CAL_TIMEZONE, 0x05);
    assert_eq!(CAL_ALL_DAY_EVENT, 0x06);
    assert_eq!(CAL_ATTENDEES, 0x07);
    assert_eq!(CAL_ATTENDEE, 0x08);
    assert_eq!(CAL_ATTENDEE_EMAIL, 0x09);
    assert_eq!(CAL_ATTENDEE_NAME, 0x0A);
    assert_eq!(CAL_BUSY_STATUS, 0x0D);
    assert_eq!(CAL_DTSTAMP, 0x11);
    assert_eq!(CAL_END_TIME, 0x12);
    assert_eq!(CAL_EXCEPTION, 0x13);
    assert_eq!(CAL_EXCEPTIONS, 0x14);
    assert_eq!(CAL_DELETED, 0x15);
    assert_eq!(CAL_EXCEPTION_START_TIME, 0x16);
    assert_eq!(CAL_LOCATION, 0x17);
    assert_eq!(CAL_MEETING_STATUS, 0x18);
    assert_eq!(CAL_ORGANIZER_EMAIL, 0x19);
    assert_eq!(CAL_ORGANIZER_NAME, 0x1A);
    assert_eq!(CAL_RECURRENCE, 0x1B);
    assert_eq!(CAL_RECURRENCE_TYPE, 0x1C);
    assert_eq!(CAL_RECURRENCE_UNTIL, 0x1D);
    assert_eq!(CAL_RECURRENCE_OCCURRENCES, 0x1E);
    assert_eq!(CAL_RECURRENCE_INTERVAL, 0x1F);
    assert_eq!(CAL_RECURRENCE_DAY_OF_WEEK, 0x20);
    assert_eq!(CAL_RECURRENCE_DAY_OF_MONTH, 0x21);
    assert_eq!(CAL_RECURRENCE_WEEK_OF_MONTH, 0x22);
    assert_eq!(CAL_RECURRENCE_MONTH_OF_YEAR, 0x23);
    assert_eq!(CAL_REMINDER, 0x24);
    assert_eq!(CAL_SENSITIVITY, 0x25);
    assert_eq!(CAL_SUBJECT, 0x26);
    assert_eq!(CAL_START_TIME, 0x27);
    assert_eq!(CAL_ATTENDEE_STATUS, 0x29);
    assert_eq!(CAL_RESPONSE_REQUESTED, 0x34);
    assert_eq!(CAL_UID, 0x28);
    assert_eq!(BASE_LOCATION, 0x20);

    // tag_name() resolution — cross-checks code_pages.rs CALENDAR_TOKENS.
    let cases: &[(u8, u8, &str)] = &[
        (PAGE_CALENDAR, CAL_TIMEZONE, "Timezone"),
        (PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "AllDayEvent"),
        (PAGE_CALENDAR, CAL_ATTENDEES, "Attendees"),
        (PAGE_CALENDAR, CAL_ATTENDEE, "Attendee"),
        (PAGE_CALENDAR, CAL_ATTENDEE_EMAIL, "Email"),
        (PAGE_CALENDAR, CAL_ATTENDEE_NAME, "Name"),
        (PAGE_CALENDAR, CAL_BUSY_STATUS, "BusyStatus"),
        (PAGE_CALENDAR, CAL_DTSTAMP, "DtStamp"),
        (PAGE_CALENDAR, CAL_END_TIME, "EndTime"),
        (PAGE_CALENDAR, CAL_EXCEPTION, "Exception"),
        (PAGE_CALENDAR, CAL_EXCEPTIONS, "Exceptions"),
        (PAGE_CALENDAR, CAL_DELETED, "Deleted"),
        (
            PAGE_CALENDAR,
            CAL_EXCEPTION_START_TIME,
            "ExceptionStartTime",
        ),
        (PAGE_CALENDAR, CAL_LOCATION, "Location"),
        (PAGE_CALENDAR, CAL_MEETING_STATUS, "MeetingStatus"),
        (PAGE_CALENDAR, CAL_ORGANIZER_EMAIL, "OrganizerEmail"),
        (PAGE_CALENDAR, CAL_ORGANIZER_NAME, "OrganizerName"),
        (PAGE_CALENDAR, CAL_RECURRENCE, "Recurrence"),
        (PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "Type"),
        (PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "Until"),
        (PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "Occurrences"),
        (PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "Interval"),
        (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "DayOfWeek"),
        (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH, "DayOfMonth"),
        (PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH, "WeekOfMonth"),
        (PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR, "MonthOfYear"),
        (PAGE_CALENDAR, CAL_REMINDER, "Reminder"),
        (PAGE_CALENDAR, CAL_SENSITIVITY, "Sensitivity"),
        (PAGE_CALENDAR, CAL_SUBJECT, "Subject"),
        (PAGE_CALENDAR, CAL_START_TIME, "StartTime"),
        (PAGE_CALENDAR, CAL_ATTENDEE_STATUS, "AttendeeStatus"),
        (PAGE_CALENDAR, CAL_RESPONSE_REQUESTED, "ResponseRequested"),
        (PAGE_CALENDAR, CAL_UID, "UID"),
        (pages::BASE, BASE_LOCATION, "Location"),
        (pages::BASE, base::DISPLAY_NAME, "DisplayName"),
    ];
    for &(page, token, name) in cases {
        assert_eq!(
            WbxmlElement::empty(page, token).tag_name(),
            name,
            "({page}, 0x{token:02X}) must resolve to {name}"
        );
    }
    // Body children used by the parser (page 17, §2.1.2.1.18) plus
    // the Location container's DisplayName child (M8-L1).
    assert_eq!(base::BODY, 0x0A);
    assert_eq!(base::TYPE, 0x06);
    assert_eq!(base::DATA, 0x0B);
    assert_eq!(base::DISPLAY_NAME, 0x10);
}

/// Golden TZI fixtures — base64 of the 172-byte [MS-ASDTYPE] §2.7.6
/// TimeZone structure:
/// ```text
/// Bias(4, i32 LE) | StandardName(64) | StandardDate(16, SYSTEMTIME
/// 8×u16 LE: wYear wMonth wDayOfWeek wDay wHour wMinute wSecond
/// wMilliseconds) | StandardBias(4, i32 LE) | DaylightName(64) |
/// DaylightDate(16) | DaylightBias(4, i32 LE)
/// ```
/// (a) Flat UTC+8 (China Standard Time shape): Bias = -480
/// (`20 FE FF FF`), both SYSTEMTIMEs zeroed (no DST), both rule biases
/// 0, names zeroed. `pub(crate)` (M8 Task 4) so the class-aware Sync
/// seam tests in `commands/sync/tests.rs` reuse the SAME golden blob — no
/// transcription copy.
pub(crate) const TZI_FLAT_UTC8: &str = "IP7//wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

/// Fixture: a fully-populated Calendar ApplicationData covering every
/// core field plus the Task-3 containers. Token layout (page, token):
/// ```text
/// ApplicationData (0, 0x1D)
///   ├── Subject         (4, 0x26) = "Weekly Sync"
///   ├── Location        (4, 0x17) = "Room 42"          (≤14.1 form)
///   ├── StartTime       (4, 0x27) = "20260818T090000Z"
///   ├── EndTime         (4, 0x12) = "20260818T100000Z"
///   ├── DtStamp         (4, 0x11) = "20260815T120000Z"
///   ├── AllDayEvent     (4, 0x06) = "0"
///   ├── OrganizerName   (4, 0x1A) = "Felix Zhou"
///   ├── OrganizerEmail  (4, 0x19) = "felixzhou@kylins.local"
///   ├── Sensitivity     (4, 0x25) = "2"   (Private)
///   ├── BusyStatus      (4, 0x0D) = "2"   (Busy)
///   ├── Reminder        (4, 0x24) = "15"  (minutes)
///   ├── MeetingStatus   (4, 0x18) = "1"   (meeting, user organizes)
///   ├── ResponseRequested (4, 0x34) = "1"
///   ├── Timezone        (4, 0x05) = TZI_FLAT_UTC8 (valid 172-byte blob)
///   ├── Body            (17, 0x0A)
///   │     ├── Type      (17, 0x06) = "1"  (PlainText)
///   │     └── Data      (17, 0x0B) = "Agenda: sync status"
///   ├── Attendees       (4, 0x07)
///   │     ├── Attendee  (4, 0x08) { Name "Bob", Email, Status 3 }
///   │     └── Attendee  (4, 0x08) { Name "Carol", Email, Type 1 }
///   ├── Recurrence      (4, 0x1B) { Type 1, Interval 1, DayOfWeek 62,
///   │                               Until 20261225T090000Z }
///   └── Exceptions      (4, 0x14)
///         ├── Exception (4, 0x13) { ExceptionStartTime, Deleted 1 }
///         └── Exception (4, 0x13) { ExceptionStartTime, StartTime,
///                                   EndTime, Subject, Location,
///                                   AllDayEvent 0 }
/// ```
/// `pub(crate)` (M8 Task 4) so the class-aware Sync seam tests in
/// `commands/sync/tests.rs` build their Add fixture from this exact tree —
/// one source of truth for the golden wire shape.
pub(crate) fn fixture_full_app_data() -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Weekly Sync"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_LOCATION, "Room 42"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "20260818T090000Z"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260818T100000Z"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_DTSTAMP, "20260815T120000Z"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "0"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_ORGANIZER_NAME, "Felix Zhou"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_ORGANIZER_EMAIL, "felixzhou@kylins.local"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_SENSITIVITY, "2"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_BUSY_STATUS, "2"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_REMINDER, "15"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_MEETING_STATUS, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RESPONSE_REQUESTED, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_TIMEZONE, TZI_FLAT_UTC8),
            WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "1"),
                    WbxmlElement::text(pages::BASE, base::DATA, "Agenda: sync status"),
                ],
            ),
            WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_ATTENDEES,
                vec![
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_ATTENDEE,
                        vec![
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ATTENDEE_NAME, "Bob"),
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_ATTENDEE_EMAIL,
                                "bob@example.com",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ATTENDEE_STATUS, "3"),
                        ],
                    ),
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_ATTENDEE,
                        vec![
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ATTENDEE_NAME, "Carol"),
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_ATTENDEE_EMAIL,
                                "carol@example.com",
                            ),
                            // AttendeeType (0x2A) — not modeled by the
                            // v1 struct; must be skipped without error.
                            WbxmlElement::text(PAGE_CALENDAR, 0x2A, "1"),
                        ],
                    ),
                ],
            ),
            WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_RECURRENCE,
                vec![
                    WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "1"),
                    WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
                    WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "62"),
                    WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "20261225T090000Z"),
                ],
            ),
            WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_EXCEPTIONS,
                vec![
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20260825T090000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_DELETED, "1"),
                        ],
                    ),
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20260901T090000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "20260901T100000Z"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260901T110000Z"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Moved"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_LOCATION, "Room 7"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "0"),
                        ],
                    ),
                ],
            ),
        ],
    )
}
