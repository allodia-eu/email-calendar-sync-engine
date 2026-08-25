// SPDX-License-Identifier: MPL-2.0
//! MS-ASCAL Calendar-class item model ([MS-ASCAL] §2.2) + downsync parse of a
//! Calendar-class `ApplicationData` element.
//!
//! Token fidelity red line: every page-4 token value below was looked up in
//! `docs/Exchange/MS-ASWBXML.txt` §2.1.2.1.5 ("Code Page 4: Calendar",
//! v20220429) and cross-checked against the same table in
//! `docs/Exchange/[MS-ASWBXML].pdf` — never from memory. The values match
//! `CALENDAR_TOKENS` in `wbxml/code_pages.rs`. Element value semantics per
//! [MS-ASCAL] §2.2.2 (`docs/Exchange/[MS-ASCAL].pdf`) and [MS-ASDTYPE]
//! (§2.1 boolean `"0"`/`"1"`, §2.7.2 Compact DateTime, §2.7.6 TimeZone).
//!
//! Downsync only: v1 never BUILDS Calendar-class items for upload.
//!
//! Parse policy (the Email `parse_application_data` precedent in
//! `commands/sync.rs`): malformed values → `log::warn!` with the element
//! name + offending text, then default — never panic, never swallow
//! silently; tokens this task does not model → `log::debug!` skip.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};

use crate::wbxml::{
    WbxmlElement, WbxmlError, WbxmlValue,
    tags::{base, pages},
};

// ============================================================================
// Code-page tag constants
// ============================================================================

/// Code page 4 = Calendar ([MS-ASWBXML] §2.1.2.1.5).
pub const PAGE_CALENDAR: u8 = 4;

// --- Page 4 (Calendar) tokens — [MS-ASWBXML] §2.1.2.1.5 table -------------
// (docs/Exchange/MS-ASWBXML.txt, "2.1.2.1.5 Code Page 4: Calendar";
// 0x0B/0x0C are the 2.5-only calendar Body/BodyTruncated — unused since 12.0,
// not modeled; 0x10 is unassigned.)

/// `Timezone` = 0x05 (all versions). Value: [MS-ASDTYPE] §2.7.6 TimeZone
/// structure — a base64 blob carried in a string element.
pub const CAL_TIMEZONE: u8 = 0x05;
/// `AllDayEvent` = 0x06 (all versions). unsignedByte 0|1 ([MS-ASCAL] §2.2.2.1).
pub const CAL_ALL_DAY_EVENT: u8 = 0x06;
/// `Attendees` = 0x07 (all versions). Container of `Attendee` children
/// ([MS-ASCAL] §2.2.2.4).
pub const CAL_ATTENDEES: u8 = 0x07;
/// `Attendee` = 0x08 (all versions). Container child of `Attendees`
/// ([MS-ASCAL] §2.2.2.3).
pub const CAL_ATTENDEE: u8 = 0x08;
/// `Email` = 0x09 (all versions). Required child of `Attendee` — the
/// attendee's e-mail address ([MS-ASCAL] §2.2.2.19).
pub const CAL_ATTENDEE_EMAIL: u8 = 0x09;
/// `Name` = 0x0A (all versions). Required child of `Attendee` — the
/// attendee's display name ([MS-ASCAL] §2.2.2.30).
pub const CAL_ATTENDEE_NAME: u8 = 0x0A;
/// `BusyStatus` = 0x0D (all versions). unsignedByte 0..=4 ([MS-ASCAL] §2.2.2.9).
pub const CAL_BUSY_STATUS: u8 = 0x0D;
/// `DtStamp` = 0x11 (all versions). Compact DateTime ([MS-ASCAL] §2.2.2.18).
pub const CAL_DTSTAMP: u8 = 0x11;
/// `EndTime` = 0x12 (all versions). Compact DateTime ([MS-ASCAL] §2.2.2.20).
pub const CAL_END_TIME: u8 = 0x12;
/// `Exception` = 0x13 (all versions). One recurrence exception; child of
/// `Exceptions` ([MS-ASCAL] §2.2.2.21).
pub const CAL_EXCEPTION: u8 = 0x13;
/// `Exceptions` = 0x14 (all versions). Container of `Exception` children
/// ([MS-ASCAL] §2.2.2.22).
pub const CAL_EXCEPTIONS: u8 = 0x14;
/// `Deleted` = 0x15 (all versions). Child of `Exception`; value `"1"` marks
/// the occurrence deleted with no replacement data ([MS-ASCAL] §2.2.2.16).
pub const CAL_DELETED: u8 = 0x15;
/// `ExceptionStartTime` = 0x16 (all versions). Required child of
/// `Exception` (2.5-14.1): the original occurrence's start time, Compact
/// DateTime ([MS-ASCAL] §2.2.2.23).
pub const CAL_EXCEPTION_START_TIME: u8 = 0x16;
/// `Location` = 0x17. In 16.0/16.1 the server sends `airsyncbase:Location`
/// (page 17, `BASE_LOCATION`) instead — [MS-ASWBXML] §2.1.2.1.5 note 2 and
/// [MS-ASCAL] §2.2.2.27. Both forms are accepted.
pub const CAL_LOCATION: u8 = 0x17;
/// `MeetingStatus` = 0x18 (all versions). Bit flags M/R/C; wire values
/// {0,1,3,5,7,9,11,13,15} ([MS-ASCAL] §2.2.2.28 — 13 = "Same as 5":
/// cancelled, user is the organizer).
pub const CAL_MEETING_STATUS: u8 = 0x18;
/// `OrganizerEmail` = 0x19 (all versions). String, e-mail address format
/// ([MS-ASCAL] §2.2.2.35).
pub const CAL_ORGANIZER_EMAIL: u8 = 0x19;
/// `OrganizerName` = 0x1A (all versions). String ([MS-ASCAL] §2.2.2.36).
pub const CAL_ORGANIZER_NAME: u8 = 0x1A;
/// `Recurrence` = 0x1B (all versions). Container of the recurrence pattern
/// ([MS-ASCAL] §2.2.2.37).
pub const CAL_RECURRENCE: u8 = 0x1B;
/// `Type` = 0x1C (all versions). Child of `Recurrence`: recurrence kind —
/// enum {0,1,2,3,5,6} per [MS-ASCAL] §2.2.2.45 v20220429 (see
/// [`CalendarRecurrence::recurrence_type`]).
pub const CAL_RECURRENCE_TYPE: u8 = 0x1C;
/// `Until` = 0x1D (all versions). Child of `Recurrence`: start time of the
/// last instance, Compact DateTime string; mutually exclusive with
/// `Occurrences` ([MS-ASCAL] §2.2.2.47).
pub const CAL_RECURRENCE_UNTIL: u8 = 0x1D;
/// `Occurrences` = 0x1E (all versions). Child of `Recurrence`: instance
/// count, unsignedShort ≤ 999; mutually exclusive with `Until`
/// ([MS-ASCAL] §2.2.2.32).
pub const CAL_RECURRENCE_OCCURRENCES: u8 = 0x1E;
/// `Interval` = 0x1F (all versions). Child of `Recurrence`: gap between
/// recurrences, unsignedShort 0-999 ([MS-ASCAL] §2.2.2.25).
pub const CAL_RECURRENCE_INTERVAL: u8 = 0x1F;
/// `DayOfWeek` = 0x20 (all versions). Child of `Recurrence`: bitmask
/// 1=Sun…64=Sat plus specials 62/65/127, ≤ 127 ([MS-ASCAL] §2.2.2.15).
pub const CAL_RECURRENCE_DAY_OF_WEEK: u8 = 0x20;
/// `DayOfMonth` = 0x21 (all versions). Child of `Recurrence`: 1-31
/// ([MS-ASCAL] §2.2.2.14).
pub const CAL_RECURRENCE_DAY_OF_MONTH: u8 = 0x21;
/// `WeekOfMonth` = 0x22 (all versions). Child of `Recurrence`: 1-4, 5=last
/// ([MS-ASCAL] §2.2.2.48).
pub const CAL_RECURRENCE_WEEK_OF_MONTH: u8 = 0x22;
/// `MonthOfYear` = 0x23 (all versions). Child of `Recurrence`: 1-12
/// ([MS-ASCAL] §2.2.2.29).
pub const CAL_RECURRENCE_MONTH_OF_YEAR: u8 = 0x23;
/// `Reminder` = 0x24 (all versions). unsignedInt minutes, or an EmptyTag in
/// 16.x meaning "no reminder" ([MS-ASCAL] §2.2.2.38).
pub const CAL_REMINDER: u8 = 0x24;
/// `Sensitivity` = 0x25 (all versions). unsignedByte 0..=3
/// ([MS-ASCAL] §2.2.2.41).
pub const CAL_SENSITIVITY: u8 = 0x25;
/// `Subject` = 0x26 (all versions). String ([MS-ASCAL] §2.2.2.43).
pub const CAL_SUBJECT: u8 = 0x26;
/// `StartTime` = 0x27 (all versions). Compact DateTime
/// ([MS-ASCAL] §2.2.2.42).
pub const CAL_START_TIME: u8 = 0x27;
/// `AttendeeStatus` = 0x29 (12.0-16.1). Child of `Attendee`: acceptance
/// enum {0,2,3,4,5} per [MS-ASCAL] §2.2.2.5 (see
/// [`CalendarAttendee::status`]).
pub const CAL_ATTENDEE_STATUS: u8 = 0x29;
/// `ResponseRequested` = 0x34 (14.0+). boolean ([MS-ASCAL] §2.2.2.39).
pub const CAL_RESPONSE_REQUESTED: u8 = 0x34;
/// `UID` = 0x28 (12.0+). String, max 300 chars ([MS-ASCAL] §2.2.2.46) — the
/// item's GlobalObjectId identity. At 16.x the SAME value travels inside an
/// invite mail's `email:MeetingRequest` ([MS-ASWBXML] §2.1.2.1.4 note 4 —
/// the Calendar-page UID replaces the Email-page GlobalObjId at 16.0/16.1),
/// which is the exact-key invite↔event correlation [MS-ASEMAIL] §3.1.4.7
/// prescribes. NOT the store row identity (`calendar_events.uid` = the EAS
/// ServerId).
pub const CAL_UID: u8 = 0x28;

// --- AirSyncBase (page 17) tokens used inside Calendar items --------------
// [MS-ASWBXML] §2.1.2.1.18 + §2.1.2.1.5 notes 1-2:
//   note 1: with 12.0+ `airsyncbase:Body` (17, 0x0A) replaces the 2.5-only
//           calendar-page Body, so 16.1 calendar bodies arrive on page 17;
//   note 2: with 16.0/16.1 `airsyncbase:Location` (17, 0x20) replaces
//           `calendar:Location` (4, 0x17).

/// `airsyncbase:Location` = 0x20 (16.0/16.1 only; [MS-ASWBXML] §2.1.2.1.18).
/// Registered in the tag registry as [`base::LOCATION`] since the M8-L1
/// variant (the email `MeetingRequest` parse reads the same token); the
/// local name keeps the pinned-test vocabulary.
const BASE_LOCATION: u8 = base::LOCATION;

// ============================================================================
// Model types ([MS-ASCAL] §2.2)
// ============================================================================

/// One EAS Calendar item's application data (downsync model; v1 never builds
/// these for upload).
///
/// DateTime fields keep the wire string verbatim (Compact DateTime per
/// [MS-ASDTYPE] §2.7.2, e.g. `20130722T090000Z`) for golden fidelity;
/// conversion to unix-secs happens downstream (M8 Task 5/6).
///
/// Serde derives (M8 Task 4): the type rides inside `SyncResult`
/// (`CalendarItemWithId`), which itself derives `Serialize`/`Deserialize`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarEventProps {
    /// `AllDayEvent` — `true` when the wire carried `"1"` ([MS-ASCAL] §2.2.2.1).
    pub all_day_event: bool,
    /// `StartTime` raw wire value (Compact DateTime), `None` when absent or
    /// malformed ([MS-ASCAL] §2.2.2.42).
    pub start_time: Option<String>,
    /// `EndTime` raw wire value ([MS-ASCAL] §2.2.2.20).
    pub end_time: Option<String>,
    /// `DtStamp` raw wire value — UTC last-modified stamp
    /// ([MS-ASCAL] §2.2.2.18).
    pub dtstamp: Option<String>,
    /// `Subject` ([MS-ASCAL] §2.2.2.43).
    pub subject: Option<String>,
    /// `Location` — accepts both `calendar:Location` (≤14.1) and
    /// `airsyncbase:Location` (16.x) wire forms.
    pub location: Option<String>,
    /// Plain-text body: `airsyncbase:Body` with `Type = 1` (PlainText).
    /// HTML/MIME bodies are not modeled on calendar items in v1.
    pub body_plain: Option<String>,
    /// `OrganizerName` ([MS-ASCAL] §2.2.2.36).
    pub organizer_name: Option<String>,
    /// `OrganizerEmail` ([MS-ASCAL] §2.2.2.35).
    pub organizer_email: Option<String>,
    /// `Sensitivity` — 0=Normal 1=Personal 2=Private 3=Confidential
    /// ([MS-ASCAL] §2.2.2.41).
    pub sensitivity: Option<u8>,
    /// `BusyStatus` — 0=Free 1=Tentative 2=Busy 3=Out of Office
    /// 4=Working elsewhere ([MS-ASCAL] §2.2.2.9, v20220429).
    pub busy_status: Option<u8>,
    /// `Reminder` element present with a minute count ([MS-ASCAL] §2.2.2.38).
    pub reminder_set: bool,
    /// `Reminder` minutes before start ([MS-ASCAL] §2.2.2.38).
    pub reminder_minutes: Option<u32>,
    /// `MeetingStatus` — M(eeting)/R(eceived)/C(ancelled) bit flags; wire
    /// values {0,1,3,5,7,9,11,13,15} ([MS-ASCAL] §2.2.2.28; 13 = "Same as
    /// 5": cancelled, user is the organizer).
    pub meeting_status: Option<u8>,
    /// `ResponseRequested` ([MS-ASCAL] §2.2.2.39).
    pub response_requested: bool,
    /// `Timezone` — raw base64 blob plus its decoded [MS-ASDTYPE] §2.7.6
    /// structure when the blob is well-formed.
    pub time_zone: Option<TimeZoneBlob>,
    /// `Recurrence` — raw wire values per [MS-ASCAL] §2.2.2.37; conversion
    /// to RRULE is downstream (M8 Task 6), never here.
    pub recurrence: Option<CalendarRecurrence>,
    /// `UID` ([MS-ASCAL] §2.2.2.46, 12.0+) — the item's GlobalObjectId
    /// identity string (≤300 chars, verbatim). The exact-key join against
    /// an invite mail's MeetingRequest UID/converted GlobalObjId
    /// ([MS-ASEMAIL] §3.1.4.7). Distinct from the wrapper's ServerId.
    pub uid: Option<String>,
    /// `Exceptions` — deleted markers and modified occurrences
    /// ([MS-ASCAL] §2.2.2.22).
    pub exceptions: Vec<CalendarException>,
    /// `Attendees` — the invited-attendee list ([MS-ASCAL] §2.2.2.4).
    pub attendees: Vec<CalendarAttendee>,
}

/// One `Attendee` child ([MS-ASCAL] §2.2.2.3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarAttendee {
    /// `Name` child — display name ([MS-ASCAL] §2.2.2.30). Required by the
    /// spec; `None` (with a warning) when the wire omits it.
    pub name: Option<String>,
    /// `Email` child — e-mail address ([MS-ASCAL] §2.2.2.19). Required by
    /// the spec; empty (with a warning) when the wire omits it.
    pub email: String,
    /// `AttendeeStatus` — 0=Response unknown 2=Tentative 3=Accept
    /// 4=Decline 5=Not responded ([MS-ASCAL] §2.2.2.5). Values outside the
    /// enum warn but are kept raw (downsync fidelity); non-numeric values
    /// warn + `None`.
    pub status: Option<u8>,
}

/// Recurrence container ([MS-ASCAL] §2.2.2.37) — raw wire values only;
/// RRULE conversion is M8 Task 6. Children not modeled here
/// (`CalendarType`/`IsLeapMonth`/`FirstDayOfWeek`) are debug-skipped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarRecurrence {
    /// `Type` ([MS-ASCAL] §2.2.2.45, v20220429): 0=daily, 1=weekly,
    /// 2=monthly (day of month), 3=monthly (nth weekday), 5=yearly (date),
    /// 6=yearly (nth weekday). The 2022 enum has NO value 4 and NO
    /// regenerate-after-done variants; out-of-enum wire values warn but are
    /// kept raw. Absent Type warns and defaults to 0.
    pub recurrence_type: u8,
    /// `Interval` — gap between recurrences; spec range 0-999
    /// ([MS-ASCAL] §2.2.2.25).
    pub interval: Option<u32>,
    /// `DayOfWeek` bitmask: 1=Sunday 2=Monday 4=Tuesday 8=Wednesday
    /// 16=Thursday 32=Friday 62=weekdays 64=Saturday 65=weekend days
    /// 127=last day of the month; sums allowed, MUST be ≤ 127
    /// ([MS-ASCAL] §2.2.2.15).
    pub day_of_week: Option<u32>,
    /// `DayOfMonth` — 1-31 ([MS-ASCAL] §2.2.2.14).
    pub day_of_month: Option<u32>,
    /// `WeekOfMonth` — 1-4, 5=last week of the month
    /// ([MS-ASCAL] §2.2.2.48).
    pub week_of_month: Option<u32>,
    /// `MonthOfYear` — 1-12 ([MS-ASCAL] §2.2.2.29).
    pub month_of_year: Option<u32>,
    /// `Until` — start time of the last instance as carried on the wire
    /// (Compact DateTime string, [MS-ASCAL] §2.2.2.47). Mutually exclusive
    /// with `occurrences`.
    pub until: Option<String>,
    /// `Occurrences` — instance count before the series ends; spec max 999
    /// ([MS-ASCAL] §2.2.2.32). Mutually exclusive with `until`.
    pub occurrences: Option<u32>,
    /// Derived, not a wire token: `true` when neither `Until` nor
    /// `Occurrences` is present — "If neither value is set, the event has
    /// no end date" ([MS-ASCAL] §2.2.2.37.1).
    pub no_end: bool,
}

/// One recurrence exception ([MS-ASCAL] §2.2.2.21). A deleted marker
/// carries no replacement data; a modified occurrence carries its own
/// subset of the event fields. Children not modeled here (Sensitivity,
/// BusyStatus, Reminder, MeetingStatus, DtStamp, Categories, Attendees,
/// InstanceId, …) are debug-skipped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarException {
    /// Deleted-occurrence marker: `Deleted` carried value `"1"`
    /// ([MS-ASCAL] §2.2.2.16).
    pub deleted: bool,
    /// `ExceptionStartTime` — start time of the ORIGINAL occurrence being
    /// replaced ([MS-ASCAL] §2.2.2.23); required in 2.5-14.1.
    pub exception_start_time: Option<String>,
    /// Modified-occurrence fields (subset of the top-level event props).
    /// Modified occurrence start (`calendar:StartTime`, when present).
    pub start_time: Option<String>,
    /// Modified occurrence end (`calendar:EndTime`, when present).
    pub end_time: Option<String>,
    /// Modified occurrence subject (`calendar:Subject`, when present).
    pub subject: Option<String>,
    /// Modified occurrence location (`calendar:Location`, when present).
    pub location: Option<String>,
    /// Modified occurrence body (`calendar:Body`, plain text, when present).
    pub body_plain: Option<String>,
    /// `AllDayEvent` — OPTIONAL inside an Exception ([MS-ASCAL] §2.2.2.21).
    /// `Some(true)` / `Some(false)` when the wire carried `"1"` / `"0"`;
    /// `None` when the element was ABSENT — an omitted child means the
    /// occurrence keeps the series' all-day-ness, and the converter
    /// inherits the series-level flag. A present-but-unreadable value
    /// warns and degrades to `None` (same fallback as absence, loudly).
    pub all_day_event: Option<bool>,
}

/// `Timezone` payload: the raw base64 string plus its decoded
/// [MS-ASDTYPE] §2.7.6 TimeZone structure.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeZoneBlob {
    /// The base64 string exactly as the wire carried it.
    pub raw_base64: Option<String>,
    /// Decoded structure; `None` when the blob is malformed (warned, never
    /// panics).
    pub parsed: Option<TziTimeZone>,
}

/// Decoded [MS-ASDTYPE] §2.7.6 TimeZone structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TziTimeZone {
    /// `Bias` field — offset from UTC in minutes with sign UTC − local
    /// ([MS-ASDTYPE] §2.7.6: Pacific (UTC-8) is +480, so UTC+8 is -480).
    pub base_bias_minutes: i32,
    /// Standard-time transition rule — `StandardDate` + `StandardBias`;
    /// `None` when the SYSTEMTIME is zeroed/inactive (no DST).
    pub standard: Option<TziRule>,
    /// Daylight-time transition rule — `DaylightDate` + `DaylightBias`;
    /// `None` when zeroed/inactive.
    pub daylight: Option<TziRule>,
}

/// One yearly DST transition: the rule's bias offset plus the SYSTEMTIME
/// fields ([MS-DTYP] §2.3.13 layout: wYear wMonth wDayOfWeek wDay wHour
/// wMinute wSecond wMilliseconds, all little-endian u16). wYear is not
/// modeled — recurring transitions carry 0; a non-zero year is debug-logged
/// and ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TziRule {
    /// Minutes added to [`TziTimeZone::base_bias_minutes`] while this rule
    /// is in effect (`StandardBias` / `DaylightBias`).
    pub bias_offset_minutes: i32,
    /// Transition month, 1-12 (SYSTEMTIME wMonth).
    pub month: u16,
    /// Transition weekday, 0=Sunday (SYSTEMTIME wDayOfWeek).
    pub day_of_week: u16,
    /// Occurrence of that weekday in the month, 1-5, 5=last (SYSTEMTIME
    /// wDay).
    pub day_occurrence: u16,
    /// Transition hour, local time (SYSTEMTIME wHour).
    pub hour: u16,
    /// Transition minute (SYSTEMTIME wMinute).
    pub minute: u16,
}

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

// ============================================================================
// Field-parse helpers (permissive: warn + default, never panic — the
// ApplicationData precedent from commands/sync.rs)
// ============================================================================

/// Permissive text extraction — the `commands::text_value_opt` twin:
/// missing or non-text values map to `None` rather than aborting the item
/// parse. (Local copy because the commands-module helper is private.)
fn text_value_opt(elem: &WbxmlElement) -> Option<String> {
    match &elem.value {
        WbxmlValue::Text(s) => Some(s.clone()),
        WbxmlValue::Opaque(b) => std::str::from_utf8(b)
            .ok()
            .map(std::string::ToString::to_string),
        WbxmlValue::Empty => None,
    }
}

/// Boolean per [MS-ASDTYPE] §2.1: `"1"` = true, `"0"` = false. Absent →
/// default `false`; any other text → warn + `false` (loud, never silent).
fn parse_bool_field(name: &'static str, elem: &WbxmlElement) -> bool {
    match text_value_opt(elem).as_deref() {
        Some("1") => true,
        Some("0") | None => false,
        Some(other) => {
            log::warn!(
                "calendar ApplicationData: malformed {name} \"{other}\"; \
                 expected \"0\" or \"1\", defaulting to false"
            );
            false
        }
    }
}

/// Tri-state boolean for OPTIONAL wire elements whose ABSENCE is
/// semantically different from `"0"` (the Exception-level AllDayEvent,
/// [MS-ASCAL] §2.2.2.21): `"1"` → `Some(true)`, `"0"` → `Some(false)`.
/// Present-but-unreadable (other text, or an element without text) →
/// warn + `None` — an unreadable value carries no information, so the
/// caller falls back exactly like absence (loud, never silent, never a
/// forced default). Element-level ABSENCE is the caller's detection:
/// the field simply stays at its `None` default.
fn parse_tri_bool_field(name: &'static str, elem: &WbxmlElement) -> Option<bool> {
    match text_value_opt(elem).as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        Some(other) => {
            log::warn!(
                "calendar ApplicationData: malformed {name} \"{other}\"; expected \
                 \"0\" or \"1\", treating the element as absent"
            );
            None
        }
        None => {
            log::warn!(
                "calendar ApplicationData: {name} element without a text value; \
                 treating it as absent"
            );
            None
        }
    }
}

/// DateTime per [MS-ASCAL] §2.2.2.42/§2.2.2.20/§2.2.2.18 — the raw wire
/// string when it validates, else warn + `None`.
fn parse_datetime_field(name: &'static str, elem: &WbxmlElement) -> Option<String> {
    let raw = text_value_opt(elem)?;
    if is_valid_eas_datetime(&raw) {
        Some(raw)
    } else {
        log::warn!(
            "calendar ApplicationData: malformed {name} \"{raw}\"; expected Compact \
             DateTime per [MS-ASDTYPE] §2.7.2, ignoring"
        );
        None
    }
}

/// Numeric enum: parse to u8, check against the spec value set, otherwise
/// warn + `None`. Absent → `None`.
fn parse_enum_field(
    name: &'static str,
    elem: &WbxmlElement,
    valid: impl Fn(u8) -> bool,
) -> Option<u8> {
    let raw = text_value_opt(elem)?;
    match raw.parse::<u8>() {
        Ok(n) if valid(n) => Some(n),
        Ok(n) => {
            log::warn!(
                "calendar ApplicationData: {name} value {n} outside the [MS-ASCAL] \
                 range; ignoring"
            );
            None
        }
        Err(_) => {
            log::warn!(
                "calendar ApplicationData: malformed {name} \"{raw}\"; expected a \
                 number, ignoring"
            );
            None
        }
    }
}

/// `airsyncbase:Body` on a calendar item → the plain-text payload, if any.
/// Type 1 (PlainText) fills `body_plain`; Type 2 (HTML) / Type 4 (MIME) are
/// valid wire data but not modeled on calendar items in v1 (debug-logged);
/// a Body without a parseable Type warns and keeps the data as plain
/// (graceful degradation, the Email `parse_body` precedent).
fn parse_calendar_body(elem: &WbxmlElement) -> Option<String> {
    let mut body_type: Option<u8> = None;
    let mut data: Option<String> = None;
    for child in &elem.children {
        match child.tag_name() {
            "Type" => body_type = text_value_opt(child).and_then(|s| s.parse().ok()),
            "Data" => data = text_value_opt(child),
            "EstimatedDataSize" | "Truncated" => {} // not surfaced on calendar items
            _ => {
                log::debug!(
                    "calendar ApplicationData: skipping unexpected Body child {} \
                     (page {} token 0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    match body_type {
        Some(1) => data,
        Some(other) => {
            log::debug!(
                "calendar ApplicationData: Body Type {other} (not PlainText) — \
                 calendar bodies are plain-only in v1; skipping payload"
            );
            None
        }
        None => {
            if data.is_some() {
                log::warn!(
                    "calendar ApplicationData: Body without a parseable Type; \
                     keeping payload as plain text"
                );
            }
            data
        }
    }
}

/// `airsyncbase:Location` (page 17, 0x20; 16.0/16.1 only) is a CONTAINER
/// element — [MS-ASAIRS] §2.2.2.28: "The Location element is container data
/// type" — whose human-readable value lives in its `DisplayName` child
/// ([MS-ASAIRS] §2.2.2.22.3, string, "MUST have a maximum of one", 14.1+).
/// All the other §2.2.2.28 children (LocationUri, Accuracy, Latitude,
/// Longitude, Altitude, AltitudeAccuracy, Annotation, Street, City, State,
/// Country, PostalCode) are optional structured-location data the v1 model
/// does not carry — debug-skipped.
///
/// M8-L1 (2026-08-17 live seed drill): the original parser read the
/// container's own (always empty) text via `text_value_opt`, silently
/// dropping LOCATION for every real-Exchange-16.x event — hence the
/// DisplayName-first shape here. M8-L1 variant: the email `MeetingRequest`
/// parse (`commands/sync.rs`) reads the same page-17 container through
/// this helper with `ctx = "email MeetingRequest"` — calendar and email
/// share one Location parse policy.
///
/// `ctx` is the log-context prefix (which parser emitted the line) —
/// e.g. `"calendar ApplicationData"` or `"email MeetingRequest"`.
///
/// Degrades loudly, never panics, never invents a value:
/// * duplicate DisplayName children (spec violation) → warn, last wins;
/// * a container without a DisplayName (legal: every child is optional; an empty container is the
///   "no/cleared location" shape) → `None` with a debug note;
/// * defensive fallback: a leaf Location carrying text directly (a gateway serializing the ≤14.1
///   shape on page 17) still parses — pinned by `parse_location_accepts_airsyncbase_16_1_form`.
pub(crate) fn parse_location_16x(ctx: &'static str, elem: &WbxmlElement) -> Option<String> {
    if let Some(text) = text_value_opt(elem) {
        return Some(text);
    }
    let mut display: Option<String> = None;
    let mut display_seen = false;
    let mut other_children = 0usize;
    for child in &elem.children {
        if let (pages::BASE, base::DISPLAY_NAME) = (child.page, child.token) {
            if display_seen {
                log::warn!(
                    "{ctx}: Location carries more than one \
                     DisplayName child — [MS-ASAIRS] §2.2.2.22.3 allows at most one; \
                     keeping the last"
                );
            }
            display_seen = true;
            display = text_value_opt(child);
        } else {
            other_children += 1;
            log::debug!(
                "{ctx}: Location: skipping unmodeled child {} \
                 (page {} token 0x{:02X}) — v1 models only the DisplayName",
                tag_label(child),
                child.page,
                child.token
            );
        }
    }
    if !display_seen {
        log::debug!(
            "{ctx}: Location container without a DisplayName child \
             ({other_children} other child(ren)); location stays unset"
        );
    }
    display
}

// ============================================================================
// Container parse — Recurrence / Exceptions / Attendees (M8 Task 3)
// ============================================================================

/// Parse a `Recurrence` container ([MS-ASCAL] §2.2.2.37) into raw wire
/// values — RRULE conversion is downstream (M8 Task 6). Per-Type
/// requiredness (§2.2.2.37.1) is the converter's concern; here every child
/// is optional and permissive: non-numeric values warn + `None`,
/// out-of-spec-range values warn but are kept raw (downsync fidelity).
fn parse_recurrence(elem: &WbxmlElement) -> CalendarRecurrence {
    let mut rec = CalendarRecurrence::default();
    let mut type_seen = false;
    for child in &elem.children {
        match (child.page, child.token) {
            // [MS-ASCAL] §2.2.2.45 (v20220429): enum {0,1,2,3,5,6}; unknown
            // values warn but are kept raw.
            (PAGE_CALENDAR, CAL_RECURRENCE_TYPE) => {
                type_seen = true;
                match text_value_opt(child) {
                    Some(raw) => match raw.parse::<u8>() {
                        Ok(n) => {
                            if !matches!(n, 0 | 1 | 2 | 3 | 5 | 6) {
                                log::warn!(
                                    "calendar Recurrence: Type {n} outside the [MS-ASCAL] \
                                     §2.2.2.45 enum {{0,1,2,3,5,6}}; keeping raw"
                                );
                            }
                            rec.recurrence_type = n;
                        }
                        Err(_) => {
                            log::warn!(
                                "calendar Recurrence: malformed Type \"{raw}\"; expected \
                                 a number, defaulting to 0"
                            );
                        }
                    },
                    None => {
                        log::warn!(
                            "calendar Recurrence: Type element without a text value; \
                             defaulting to 0"
                        );
                    }
                }
            }
            // §2.2.2.47: Compact DateTime string, kept verbatim.
            (PAGE_CALENDAR, CAL_RECURRENCE_UNTIL) => {
                rec.until = parse_datetime_field("Recurrence/Until", child);
            }
            // §2.2.2.32: unsignedShort, max 999.
            (PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES) => {
                rec.occurrences = parse_rec_number("Occurrences", child, |n| n <= 999, "max 999");
            }
            // §2.2.2.25: unsignedShort 0-999.
            (PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL) => {
                rec.interval = parse_rec_number("Interval", child, |n| n <= 999, "0-999");
            }
            // §2.2.2.15: bitmask 1..=127 (sums of 1/2/4/8/16/32/64 plus the
            // specials 62/65/127).
            (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK) => {
                rec.day_of_week =
                    parse_rec_number("DayOfWeek", child, |n| (1..=127).contains(&n), "1-127");
            }
            // §2.2.2.14: 1-31.
            (PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH) => {
                rec.day_of_month =
                    parse_rec_number("DayOfMonth", child, |n| (1..=31).contains(&n), "1-31");
            }
            // §2.2.2.48: 1-5, 5 = last week of the month.
            (PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH) => {
                rec.week_of_month =
                    parse_rec_number("WeekOfMonth", child, |n| (1..=5).contains(&n), "1-5");
            }
            // §2.2.2.29: 1-12.
            (PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR) => {
                rec.month_of_year =
                    parse_rec_number("MonthOfYear", child, |n| (1..=12).contains(&n), "1-12");
            }
            _ => {
                // Covers CalendarType (0x37), IsLeapMonth (0x38),
                // FirstDayOfWeek (0x39) — not modeled by the v1 struct.
                log::debug!(
                    "calendar Recurrence: skipping unmodeled child {} (page {} token \
                     0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    if !type_seen {
        log::warn!(
            "calendar Recurrence: no Type child ([MS-ASCAL] §2.2.2.45 requires it in \
             protocol versions ≤ 14.1); defaulting to 0 (daily)"
        );
    }
    if rec.until.is_some() && rec.occurrences.is_some() {
        log::warn!(
            "calendar Recurrence: both Until and Occurrences present — they are \
             mutually exclusive per [MS-ASCAL] §2.2.2.47; keeping both raw"
        );
    }
    // Derived, not a wire token: "If neither value is set, the event has no
    // end date" ([MS-ASCAL] §2.2.2.37.1).
    rec.no_end = rec.until.is_none() && rec.occurrences.is_none();
    rec
}

/// Numeric `Recurrence` child: parse to u32. Non-numeric text warns +
/// `None`; parseable-but-out-of-spec-range warns and is kept raw (downsync
/// fidelity — the converter decides). `range` labels the spec range in the
/// warning.
fn parse_rec_number(
    name: &'static str,
    elem: &WbxmlElement,
    valid: impl Fn(u32) -> bool,
    range: &'static str,
) -> Option<u32> {
    let raw = text_value_opt(elem)?;
    match raw.parse::<u32>() {
        Ok(n) if valid(n) => Some(n),
        Ok(n) => {
            log::warn!(
                "calendar Recurrence: {name} value {n} outside the [MS-ASCAL] range \
                 ({range}); keeping raw"
            );
            Some(n)
        }
        Err(_) => {
            log::warn!(
                "calendar Recurrence: malformed {name} \"{raw}\"; expected a number, \
                 ignoring"
            );
            None
        }
    }
}

/// Parse an `Exceptions` container ([MS-ASCAL] §2.2.2.22) — a list of
/// `Exception` children (spec max 256, not enforced).
fn parse_exceptions(elem: &WbxmlElement) -> Vec<CalendarException> {
    let mut out = Vec::new();
    for child in &elem.children {
        match (child.page, child.token) {
            (PAGE_CALENDAR, CAL_EXCEPTION) => out.push(parse_exception(child)),
            _ => {
                log::debug!(
                    "calendar Exceptions: skipping unexpected child {} (page {} token \
                     0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    out
}

/// Parse one `Exception` element ([MS-ASCAL] §2.2.2.21). Deleted markers
/// ([MS-ASCAL] §2.2.2.16) carry no replacement data; modified occurrences
/// carry their own subset of the event fields.
fn parse_exception(elem: &WbxmlElement) -> CalendarException {
    let mut exc = CalendarException::default();
    let mut est_seen = false;
    for child in &elem.children {
        match (child.page, child.token) {
            // §2.2.2.16: value "1" marks the occurrence deleted.
            (PAGE_CALENDAR, CAL_DELETED) => {
                exc.deleted = parse_bool_field("Deleted", child);
            }
            // §2.2.2.23: the ORIGINAL occurrence's start time.
            (PAGE_CALENDAR, CAL_EXCEPTION_START_TIME) => {
                est_seen = true;
                exc.exception_start_time = parse_datetime_field("ExceptionStartTime", child);
            }
            (PAGE_CALENDAR, CAL_SUBJECT) => exc.subject = text_value_opt(child),
            (PAGE_CALENDAR, CAL_START_TIME) => {
                exc.start_time = parse_datetime_field("Exception/StartTime", child);
            }
            (PAGE_CALENDAR, CAL_END_TIME) => {
                exc.end_time = parse_datetime_field("Exception/EndTime", child);
            }
            // §2.2.2.21: AllDayEvent is OPTIONAL inside an Exception —
            // presence detection, not value defaulting (absence stays
            // `None` and the converter inherits the series-level flag).
            (PAGE_CALENDAR, CAL_ALL_DAY_EVENT) => {
                exc.all_day_event = parse_tri_bool_field("Exception/AllDayEvent", child);
            }
            // Both Location wire forms are legal Exception children
            // (§2.2.2.21): the ≤14.1 plain-text leaf, and the 16.x
            // AirSyncBase container (M8-L1).
            (PAGE_CALENDAR, CAL_LOCATION) => exc.location = text_value_opt(child),
            (pages::BASE, BASE_LOCATION) => {
                exc.location = parse_location_16x("calendar ApplicationData", child);
            }
            // 12.0+ exceptions carry airsyncbase:Body (§2.2.2.21).
            (pages::BASE, base::BODY) => exc.body_plain = parse_calendar_body(child),
            _ => {
                // Covers Sensitivity, BusyStatus, Reminder, MeetingStatus,
                // DtStamp, Categories, Attendees, UID, InstanceId, …
                log::debug!(
                    "calendar Exception: skipping unmodeled child {} (page {} token \
                     0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    if !est_seen {
        log::warn!(
            "calendar Exception: missing ExceptionStartTime (required in protocol \
             versions 2.5-14.1 per [MS-ASCAL] §2.2.2.23); keeping the exception as \
             parsed"
        );
    }
    exc
}

/// Parse an `Attendees` container ([MS-ASCAL] §2.2.2.4) — a list of
/// `Attendee` children.
fn parse_attendees(elem: &WbxmlElement) -> Vec<CalendarAttendee> {
    let mut out = Vec::new();
    for child in &elem.children {
        match (child.page, child.token) {
            (PAGE_CALENDAR, CAL_ATTENDEE) => out.push(parse_attendee(child)),
            _ => {
                log::debug!(
                    "calendar Attendees: skipping unexpected child {} (page {} token \
                     0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    out
}

/// Parse one `Attendee` element ([MS-ASCAL] §2.2.2.3). Email and Name are
/// spec-required; absence warns and degrades (empty / `None`).
fn parse_attendee(elem: &WbxmlElement) -> CalendarAttendee {
    let mut att = CalendarAttendee::default();
    let mut email_seen = false;
    let mut name_seen = false;
    for child in &elem.children {
        match (child.page, child.token) {
            (PAGE_CALENDAR, CAL_ATTENDEE_EMAIL) => {
                email_seen = true;
                att.email = text_value_opt(child).unwrap_or_default();
            }
            (PAGE_CALENDAR, CAL_ATTENDEE_NAME) => {
                name_seen = true;
                att.name = text_value_opt(child);
            }
            (PAGE_CALENDAR, CAL_ATTENDEE_STATUS) => {
                att.status = parse_attendee_status(child);
            }
            _ => {
                // Covers AttendeeType (0x2A, §2.2.2.6) and the proposed-time
                // elements — not modeled by the v1 struct.
                log::debug!(
                    "calendar Attendee: skipping unmodeled child {} (page {} token \
                     0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    if !email_seen {
        log::warn!(
            "calendar Attendee: missing Email (required by [MS-ASCAL] §2.2.2.3); \
             keeping an empty address"
        );
    }
    if !name_seen {
        log::warn!(
            "calendar Attendee: missing Name (required by [MS-ASCAL] §2.2.2.3); \
             keeping None"
        );
    }
    att
}

/// `AttendeeStatus` ([MS-ASCAL] §2.2.2.5): enum {0,2,3,4,5}. Out-of-enum
/// values warn but are kept raw (downsync fidelity); non-numeric values
/// warn + `None`.
fn parse_attendee_status(elem: &WbxmlElement) -> Option<u8> {
    let raw = text_value_opt(elem)?;
    if let Ok(n) = raw.parse::<u8>() {
        if !matches!(n, 0 | 2 | 3 | 4 | 5) {
            log::warn!(
                "calendar Attendee: AttendeeStatus {n} outside the [MS-ASCAL] \
                 §2.2.2.5 enum {{0,2,3,4,5}}; keeping raw"
            );
        }
        Some(n)
    } else {
        log::warn!(
            "calendar Attendee: malformed AttendeeStatus \"{raw}\"; expected a \
             number, ignoring"
        );
        None
    }
}

// ============================================================================
// Timezone (TZI) blob decode — [MS-ASDTYPE] §2.7.6 (M8 Task 3)
// ============================================================================

/// Byte length of the [MS-ASDTYPE] §2.7.6 TimeZone structure:
/// Bias(4) + StandardName(64) + StandardDate(16) + StandardBias(4) +
/// DaylightName(64) + DaylightDate(16) + DaylightBias(4) = 172.
const TZI_BLOB_LEN: usize = 172;

/// Decode the base64 `Timezone` blob into a [`TziTimeZone`]. Malformed
/// base64 or a wrong-length payload warn and yield `None` — the caller
/// keeps the raw string either way. Never panics.
///
/// `pub(crate)` (M8 write direction): the `calendar_write` tests reuse this
/// to pin the write→parse round-trip of the synthesized TZI blob —
/// visibility only, no logic changed (the Task-4 seam precedent above).
pub(crate) fn parse_tzi_blob(raw: &str) -> Option<TziTimeZone> {
    let bytes = match BASE64_STANDARD.decode(raw) {
        Ok(b) => b,
        Err(e) => {
            log::warn!(
                "calendar Timezone: malformed base64 ({e}); keeping the raw blob, \
                 no TZI parse"
            );
            return None;
        }
    };
    if bytes.len() != TZI_BLOB_LEN {
        log::warn!(
            "calendar Timezone: TZI blob is {} bytes; expected {TZI_BLOB_LEN} per \
             [MS-ASDTYPE] §2.7.6 — keeping the raw blob, no TZI parse",
            bytes.len()
        );
        return None;
    }
    // Field offsets follow §2.7.6 exactly; the two 64-byte name fields are
    // not modeled and skipped.
    let bias = i32_le(&bytes[0..4]);
    let standard = tzi_rule(&bytes[68..84], i32_le(&bytes[84..88]), "Standard");
    let daylight = tzi_rule(&bytes[152..168], i32_le(&bytes[168..172]), "Daylight");
    Some(TziTimeZone {
        base_bias_minutes: bias,
        standard,
        daylight,
    })
}

/// Little-endian i32 from a 4-byte slice.
fn i32_le(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Little-endian u16 from a 2-byte slice.
fn u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// Decode one 16-byte SYSTEMTIME transition ([MS-DTYP] §2.3.13: 8×
/// little-endian u16 — wYear wMonth wDayOfWeek wDay wHour wMinute wSecond
/// wMilliseconds) into a [`TziRule`].
///
/// An all-zero SYSTEMTIME means "no transition" (flat zone, no DST) →
/// `None`. wYear is ignored with a debug note — recurring transitions carry
/// 0 and the v1 model keeps only the recurring fields. Out-of-range fields
/// warn + drop the rule.
fn tzi_rule(st: &[u8], bias_offset_minutes: i32, which: &'static str) -> Option<TziRule> {
    let year = u16_le(&st[0..2]);
    let month = u16_le(&st[2..4]);
    let day_of_week = u16_le(&st[4..6]);
    let day_occurrence = u16_le(&st[6..8]);
    let hour = u16_le(&st[8..10]);
    let minute = u16_le(&st[10..12]);
    let second = u16_le(&st[12..14]);
    let millis = u16_le(&st[14..16]);
    if (
        year,
        month,
        day_of_week,
        day_occurrence,
        hour,
        minute,
        second,
        millis,
    ) == (0, 0, 0, 0, 0, 0, 0, 0)
    {
        return None; // zeroed SYSTEMTIME = rule inactive (no DST)
    }
    if year != 0 {
        log::debug!(
            "calendar Timezone: {which}Date SYSTEMTIME carries absolute year {year}; \
             the v1 model only keeps the recurring-transition fields"
        );
    }
    let valid = (1..=12).contains(&month)
        && day_of_week <= 6
        && (1..=5).contains(&day_occurrence)
        && hour <= 23
        && minute <= 59;
    if !valid {
        log::warn!(
            "calendar Timezone: {which}Date SYSTEMTIME out of range (month={month} \
             dayOfWeek={day_of_week} day={day_occurrence} hour={hour} minute={minute}); \
             dropping the rule"
        );
        return None;
    }
    Some(TziRule {
        bias_offset_minutes,
        month,
        day_of_week,
        day_occurrence,
        hour,
        minute,
    })
}

/// Validate an EAS Calendar datetime value.
///
/// Spec form: Compact DateTime ([MS-ASDTYPE] §2.7.2 ABNF) —
/// `yyyyMMdd'T'HHmmss'Z'`, e.g. `20130722T090000Z` (§3.6.2 example).
/// The RFC 3339 / ISO 8601 separated form (`2026-08-18T09:00:00Z`, optional
/// fractional seconds, `Z` or ±HH:MM offset) is accepted too: gateways and
/// captures serialize it, and the raw string is kept verbatim for golden
/// fidelity (conversion to unix-secs is downstream, M8 Task 5/6).
///
/// `pub(crate)` (M8 write direction): `calendar_write::validate` reuses this
/// as the ONE datetime-validation policy file-wide — visibility only.
pub(crate) fn is_valid_eas_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() == 16 {
        is_compact_datetime(b)
    } else {
        is_rfc3339_datetime(b)
    }
}

/// Compact form per [MS-ASDTYPE] §2.7.2. Ranges follow the ABNF literally
/// (month ≤ 12, day ≤ 31, hour ≤ 23, minute/seconds ≤ 59); no calendar-day
/// sanity (Feb 30 passes) — the wire value is kept verbatim, strictness
/// beyond the ABNF is the converter's job.
fn is_compact_datetime(b: &[u8]) -> bool {
    b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z'
        && two_digits(&b[4..6]) <= 12
        && two_digits(&b[6..8]) <= 31
        && two_digits(&b[9..11]) <= 23
        && two_digits(&b[11..13]) <= 59
        && two_digits(&b[13..15]) <= 59
}

/// Separated form: `yyyy-MM-ddTHH:mm:ss` followed by optional fractional
/// seconds and `Z` or a ±HH:MM offset.
fn is_rfc3339_datetime(b: &[u8]) -> bool {
    if b.len() < 20 {
        return false;
    }
    let shape_ok = b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[10] == b'T'
        && b[11..13].iter().all(u8::is_ascii_digit)
        && b[13] == b':'
        && b[14..16].iter().all(u8::is_ascii_digit)
        && b[16] == b':'
        && b[17..19].iter().all(u8::is_ascii_digit);
    if !shape_ok {
        return false;
    }
    if two_digits(&b[5..7]) > 12
        || two_digits(&b[8..10]) > 31
        || two_digits(&b[11..13]) > 23
        || two_digits(&b[14..16]) > 59
        || two_digits(&b[17..19]) > 59
    {
        return false;
    }
    // Fractional seconds, then zone designator.
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false; // '.' must be followed by digits
        }
    }
    match b.get(i) {
        Some(b'Z') => i + 1 == b.len(),
        Some(b'+' | b'-') => {
            let off = &b[i + 1..];
            off.len() == 5
                && off[..2].iter().all(u8::is_ascii_digit)
                && off[2] == b':'
                && off[3..].iter().all(u8::is_ascii_digit)
                && two_digits(&off[..2]) <= 23
                && two_digits(&off[3..]) <= 59
        }
        _ => false,
    }
}

/// Decode two ASCII digit bytes as a number (callers check digit-ness first).
fn two_digits(b: &[u8]) -> u32 {
    u32::from((b[0] - b'0') * 10 + (b[1] - b'0'))
}

/// Human-readable tag name for log lines — unlike `WbxmlElement::tag_name()`
/// this never warns on unregistered tokens (a skip line must not spawn a
/// second warn for the same element).
fn tag_label(elem: &WbxmlElement) -> String {
    match crate::wbxml::code_page(elem.page).and_then(|p| p.tag_name(elem.token)) {
        Some(name) => name.to_string(),
        None => format!("unknown-0x{:02X}", elem.token),
    }
}

// ============================================================================
// Tests
// ============================================================================

// `pub(crate)` (M8 Task 4): the class-aware Sync seam tests in
// `commands/sync.rs` reuse this module's golden fixture + TZI blob —
// visibility only; no test logic changed.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC};

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
    /// seam tests in `commands/sync.rs` reuse the SAME golden blob — no
    /// transcription copy.
    pub(crate) const TZI_FLAT_UTC8: &str = "IP7//wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
    /// (b) DST zone UTC+1/UTC+2 (CET/CEST shape): Bias = -60 (`C4 FF FF
    /// FF`); StandardDate = last Sunday of October at 03:00 (wMonth=10,
    /// wDayOfWeek=0, wDay=5, wHour=3) with StandardBias 0; DaylightDate =
    /// last Sunday of March at 02:00 with DaylightBias = -60 (UTC+2 while
    /// DST is in effect).
    const TZI_DST_CET: &str = "xP///wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAoAAAAFAAMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMAAAAFAAIAAAAAAAAAxP///w==";

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
    /// `commands/sync.rs` build their Add fixture from this exact tree —
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
                                WbxmlElement::text(
                                    PAGE_CALENDAR,
                                    CAL_START_TIME,
                                    "20260901T100000Z",
                                ),
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

    /// DateTime acceptance: the spec form is Compact DateTime
    /// (`20130722T090000Z`, [MS-ASDTYPE] §2.7.2/§3.6.2); the separated
    /// RFC 3339 form is accepted defensively. Range violations reject.
    #[test]
    fn parse_datetime_accepts_compact_and_rfc3339_forms() {
        let cases: &[(&str, bool)] = &[
            ("20130722T090000Z", true),          // spec §3.6.2 example
            ("20260818T235959Z", true),          // edge of day
            ("2026-08-18T09:00:00Z", true),      // separated UTC
            ("2026-08-18T09:00:00.123Z", true),  // fractional seconds
            ("2026-08-18T09:00:00+08:00", true), // numeric offset
            ("20260818T250000Z", false),         // hour 25
            ("20260818T096100Z", false),         // minute 61
            ("2026-13-18T09:00:00Z", false),     // month 13
            ("2026-08-18T09:00:00", false),      // missing zone designator
            ("2026-08-18 09:00:00Z", false),     // space instead of T
        ];
        for &(value, ok) in cases {
            let app_data = WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, value)],
            );
            let props = parse_calendar_application_data(&app_data).expect("parse ok");
            assert_eq!(
                props.start_time.as_deref() == Some(value),
                ok,
                "StartTime \"{value}\" acceptance mismatch"
            );
        }
    }

    /// 16.x wire form, LEAF shape (defensive): `airsyncbase:Location`
    /// (page 17, 0x20) per [MS-ASWBXML] §2.1.2.1.5 note 2 carrying its
    /// text directly. The spec form is a container — see
    /// [`parse_location_16x_container_reads_display_name_child`]; this
    /// test pins the tolerant fallback for gateways that serialize the
    /// ≤14.1 shape on page 17.
    #[test]
    fn parse_location_accepts_airsyncbase_16_1_form() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(
                pages::BASE,
                BASE_LOCATION,
                "Teams Meeting",
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.location.as_deref(), Some("Teams Meeting"));
    }

    // ====================================================================
    // M8-L1 (2026-08-17 live seed drill) — the 16.x Location CONTAINER
    // shape. [MS-ASAIRS] §2.2.2.28: "The Location element is container
    // data type" whose children are "all ... optional" — the
    // human-readable value is the DisplayName child (§2.2.2.22.3, max
    // one, 14.1+). All 27 drilled events (real Exchange 16.x) lost
    // LOCATION because the parser read the container's own (always
    // empty) text instead of the DisplayName child.
    // ====================================================================

    /// RED: a page-17 Location CONTAINER with a DisplayName child must
    /// yield the DisplayName text.
    #[test]
    fn parse_location_16x_container_reads_display_name_child() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                pages::BASE,
                BASE_LOCATION,
                vec![
                    WbxmlElement::text(
                        pages::BASE,
                        base::DISPLAY_NAME,
                        "Teams Room 4A, Building 2",
                    ),
                    // Structured siblings (§2.2.2.28: all optional) —
                    // unmodeled in v1, must be skipped without error.
                    // LocationUri = page 17, token 0x2C.
                    WbxmlElement::text(pages::BASE, 0x2C, "https://maps.example.com/4a"),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(
            props.location.as_deref(),
            Some("Teams Room 4A, Building 2"),
            "airsyncbase:Location container must yield its DisplayName child text"
        );
    }

    /// RED: the same container form inside an Exception (§2.2.2.28 lists
    /// `calendar:Exception` among Location's parents) must fill the
    /// exception's location.
    #[test]
    fn exception_location_16x_container_reads_display_name_child() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_EXCEPTIONS,
                vec![WbxmlElement::container(
                    PAGE_CALENDAR,
                    CAL_EXCEPTION,
                    vec![
                        WbxmlElement::text(
                            PAGE_CALENDAR,
                            CAL_EXCEPTION_START_TIME,
                            "20260901T090000Z",
                        ),
                        WbxmlElement::container(
                            pages::BASE,
                            BASE_LOCATION,
                            vec![WbxmlElement::text(
                                pages::BASE,
                                base::DISPLAY_NAME,
                                "Overflow Room B",
                            )],
                        ),
                    ],
                )],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.exceptions.len(), 1);
        assert_eq!(
            props.exceptions[0].location.as_deref(),
            Some("Overflow Room B"),
            "Exception-level airsyncbase:Location container must yield its DisplayName"
        );
    }

    /// RED: more than one DisplayName child violates [MS-ASAIRS]
    /// §2.2.2.22.3 ("MUST have a maximum of one") — warn and keep the
    /// last (the file-wide later-element-wins convention).
    #[test]
    fn location_16x_duplicate_display_name_warns_and_keeps_last() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                pages::BASE,
                BASE_LOCATION,
                vec![
                    WbxmlElement::text(pages::BASE, base::DISPLAY_NAME, "First Room"),
                    WbxmlElement::text(pages::BASE, base::DISPLAY_NAME, "Second Room"),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(
            props.location.as_deref(),
            Some("Second Room"),
            "duplicate DisplayName children must keep the last value (with a warn)"
        );
    }

    /// PIN (legal wire shapes, not errors — §2.2.2.28: every child is
    /// optional): a Location container WITHOUT a DisplayName yields
    /// `None` — never an invented value.
    #[test]
    fn location_16x_container_without_display_name_is_none() {
        for children in [
            // Structured-geo-only container (Latitude 0x27 / Longitude 0x28).
            vec![
                WbxmlElement::text(pages::BASE, 0x27, "47.641944"),
                WbxmlElement::text(pages::BASE, 0x28, "-122.127222"),
            ],
            // Empty container — the "no location / cleared" shape.
            vec![],
        ] {
            let app_data = WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::container(
                    pages::BASE,
                    BASE_LOCATION,
                    children,
                )],
            );
            let props = parse_calendar_application_data(&app_data).expect("parse ok");
            assert_eq!(
                props.location, None,
                "a DisplayName-less Location container must not invent a location"
            );
        }
    }

    /// Only a PlainText (Type 1) body fills `body_plain`; an HTML body is
    /// valid wire data but not modeled on calendar items in v1.
    #[test]
    fn parse_body_plain_only_for_type_1() {
        let html = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "2"),
                    WbxmlElement::text(pages::BASE, base::DATA, "<p>html body</p>"),
                ],
            )],
        );
        let props = parse_calendar_application_data(&html).expect("parse ok");
        assert_eq!(props.body_plain, None, "HTML calendar body is not plain");
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

    // ====================================================================
    // M8 Task 3 — Recurrence
    // ====================================================================

    /// Wrap recurrence children in an ApplicationData carrying a single
    /// `Recurrence` container.
    fn recurrence_app_data(children: Vec<WbxmlElement>) -> WbxmlElement {
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_RECURRENCE,
                children,
            )],
        )
    }

    /// Parse and unwrap the single recurrence of an ApplicationData.
    fn parse_recurrence(children: Vec<WbxmlElement>) -> CalendarRecurrence {
        let props =
            parse_calendar_application_data(&recurrence_app_data(children)).expect("parse ok");
        props
            .recurrence
            .expect("Recurrence container must parse to Some")
    }

    /// Type 0 (daily) — [MS-ASCAL] §4.4 "every other day" example; no
    /// Until/Occurrences ⇒ `no_end` is derived true (§2.2.2.37.1).
    #[test]
    fn parse_recurrence_type_0_daily_no_end() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "0"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "2"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 0,
                interval: Some(2),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Type 1 (weekly) — §4.4 "every weekday" example (DayOfWeek 62) with
    /// an Until ⇒ bounded series, `no_end` false.
    #[test]
    fn parse_recurrence_type_1_weekly_with_until() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "62"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "20261231T235959Z"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 1,
                interval: Some(1),
                day_of_week: Some(62),
                until: Some("20261231T235959Z".to_string()),
                no_end: false,
                ..Default::default()
            }
        );
    }

    /// Type 2 (monthly by day) — §4.4 "first day of every month" example,
    /// bounded by Occurrences instead of Until.
    #[test]
    fn parse_recurrence_type_2_monthly_with_occurrences() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "2"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "10"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 2,
                interval: Some(1),
                day_of_month: Some(1),
                occurrences: Some(10),
                no_end: false,
                ..Default::default()
            }
        );
    }

    /// Type 3 (monthly nth) — §4.4 "last day of every month" example:
    /// WeekOfMonth 5 (last) + DayOfWeek 127 (§2.2.2.15 special value).
    #[test]
    fn parse_recurrence_type_3_monthly_last_day() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "3"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH, "5"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "127"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 3,
                interval: Some(1),
                week_of_month: Some(5),
                day_of_week: Some(127),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Type 5 (yearly by date) — §4.4 "June 1 every year" example.
    #[test]
    fn parse_recurrence_type_5_yearly_by_date() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "5"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR, "6"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 5,
                interval: Some(1),
                day_of_month: Some(1),
                month_of_year: Some(6),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Type 6 (yearly nth weekday) — §4.4 "first Saturday of June" example.
    #[test]
    fn parse_recurrence_type_6_yearly_nth_weekday() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "6"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "64"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR, "6"),
        ]);
        assert_eq!(
            rec,
            CalendarRecurrence {
                recurrence_type: 6,
                interval: Some(1),
                week_of_month: Some(1),
                day_of_week: Some(64),
                month_of_year: Some(6),
                no_end: true,
                ..Default::default()
            }
        );
    }

    /// Out-of-enum Type: the v20220429 enum is {0,1,2,3,5,6} (§2.2.2.45 —
    /// no value 4, no regenerate variants). Unknown values WARN but are
    /// kept raw (downsync fidelity; the converter decides). Non-numeric
    /// Type warns and defaults to 0.
    #[test]
    fn parse_recurrence_unknown_type_kept_raw() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "4"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "1"),
        ]);
        assert_eq!(rec.recurrence_type, 4, "out-of-enum Type kept raw");
        assert_eq!(rec.interval, Some(1));

        let rec = parse_recurrence(vec![WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_TYPE,
            "daily",
        )]);
        assert_eq!(rec.recurrence_type, 0, "non-numeric Type defaults to 0");
        assert!(rec.no_end);
    }

    /// Out-of-spec-range numeric children warn but are kept raw; both Until
    /// and Occurrences present (mutually exclusive per §2.2.2.47) warn and
    /// both are kept, with `no_end` false.
    #[test]
    fn parse_recurrence_out_of_range_children_kept_raw() {
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "0"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "5000"), // >999
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_WEEK, "255"), // >127
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_DAY_OF_MONTH, "99"), // >31
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_WEEK_OF_MONTH, "9"), // >5
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_MONTH_OF_YEAR, "13"), // >12
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "1000"), // >999
        ]);
        assert_eq!(rec.interval, Some(5000));
        assert_eq!(rec.day_of_week, Some(255));
        assert_eq!(rec.day_of_month, Some(99));
        assert_eq!(rec.week_of_month, Some(9));
        assert_eq!(rec.month_of_year, Some(13));
        assert_eq!(rec.occurrences, Some(1000));

        // Malformed (non-numeric) children degrade to None.
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_INTERVAL, "weekly"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "not-a-date"),
        ]);
        assert_eq!(rec.interval, None);
        assert_eq!(rec.until, None);
        assert!(rec.no_end);

        // Until + Occurrences together: warn, keep both, series bounded.
        let rec = parse_recurrence(vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_TYPE, "0"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_UNTIL, "20261231T235959Z"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_RECURRENCE_OCCURRENCES, "5"),
        ]);
        assert_eq!(rec.until.as_deref(), Some("20261231T235959Z"));
        assert_eq!(rec.occurrences, Some(5));
        assert!(!rec.no_end);
    }

    // ====================================================================
    // M8 Task 3 — Exceptions
    // ====================================================================

    /// One deleted marker + one modified occurrence, exactly as the wire
    /// carries them ([MS-ASCAL] §2.2.2.16/§2.2.2.21).
    #[test]
    fn parse_exceptions_deleted_and_modified() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_EXCEPTIONS,
                vec![
                    // Deleted marker: ExceptionStartTime + Deleted=1, no
                    // replacement data.
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20260819T090000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_DELETED, "1"),
                        ],
                    ),
                    // Modified occurrence: carries its own subset fields,
                    // including an airsyncbase:Body and unmodeled children
                    // (Sensitivity here) that must be skipped without error.
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20260826T090000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Rescheduled"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "20260826T140000Z"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260826T150000Z"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_LOCATION, "Room 9"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "0"),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_SENSITIVITY, "2"),
                            WbxmlElement::container(
                                pages::BASE,
                                base::BODY,
                                vec![
                                    WbxmlElement::text(pages::BASE, base::TYPE, "1"),
                                    WbxmlElement::text(pages::BASE, base::DATA, "moved body"),
                                ],
                            ),
                        ],
                    ),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(
            props.exceptions,
            vec![
                CalendarException {
                    deleted: true,
                    exception_start_time: Some("20260819T090000Z".to_string()),
                    ..Default::default()
                },
                CalendarException {
                    deleted: false,
                    exception_start_time: Some("20260826T090000Z".to_string()),
                    start_time: Some("20260826T140000Z".to_string()),
                    end_time: Some("20260826T150000Z".to_string()),
                    subject: Some("Rescheduled".to_string()),
                    location: Some("Room 9".to_string()),
                    body_plain: Some("moved body".to_string()),
                    // AllDayEvent "0" present on the wire → Some(false);
                    // the deleted marker above omits it → None.
                    all_day_event: Some(false),
                },
            ]
        );
    }

    // ====================================================================
    // M8 Interlude A — Exception AllDayEvent absence ([MS-ASCAL]
    // §2.2.2.21: the element is OPTIONAL inside an Exception)
    // ====================================================================

    /// Absence must be distinguishable from an explicit `"0"`: `None`
    /// vs `Some(false)` — the converter inherits the series-level flag
    /// only for `None` (an omitted child means the occurrence keeps the
    /// series' all-day-ness). Before the interlude-A fix both parsed to
    /// `false`, and an all-day series' exception emitted a misshapen
    /// timed override downstream.
    #[test]
    fn exception_all_day_event_absence_parses_none_not_false() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_EXCEPTIONS,
                vec![
                    // Modified occurrence WITHOUT AllDayEvent (the element
                    // is absent on the wire).
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20260920T000000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Subject only"),
                        ],
                    ),
                    // Siblings with explicit values for contrast.
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20260927T000000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "0"),
                        ],
                    ),
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20261004T000000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "1"),
                        ],
                    ),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.exceptions.len(), 3);
        assert_eq!(
            props.exceptions[0].all_day_event, None,
            "absent AllDayEvent must parse to None (the converter inherits \
             the series flag downstream)"
        );
        assert_eq!(
            props.exceptions[1].all_day_event,
            Some(false),
            "explicit \"0\" must parse to Some(false)"
        );
        assert_eq!(
            props.exceptions[2].all_day_event,
            Some(true),
            "explicit \"1\" must parse to Some(true)"
        );
    }

    /// A present-but-unreadable exception AllDayEvent (text other than
    /// `"0"`/`"1"`, or an empty element) degrades to `None` — an
    /// unreadable value carries no information, so the converter falls
    /// back to the series flag exactly like absence (warned, never
    /// silent, never a forced timed override).
    #[test]
    fn exception_all_day_event_malformed_degrades_to_none() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
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
                                "20260920T000000Z",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "2"),
                        ],
                    ),
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_EXCEPTION,
                        vec![
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_EXCEPTION_START_TIME,
                                "20260927T000000Z",
                            ),
                            WbxmlElement::empty(PAGE_CALENDAR, CAL_ALL_DAY_EVENT),
                        ],
                    ),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.exceptions.len(), 2);
        assert_eq!(props.exceptions[0].all_day_event, None);
        assert_eq!(props.exceptions[1].all_day_event, None);
    }

    // ====================================================================
    // M8 Task 3 — Attendees
    // ====================================================================

    /// Attendee status matrix: every spec value {0,2,3,4,5} round-trips;
    /// an unknown value warns but is kept raw; absent status stays None;
    /// non-numeric status degrades to None. AttendeeType children are
    /// unmodeled and must be skipped without error.
    #[test]
    fn parse_attendees_status_matrix() {
        let attendee = |email: &str, status: Option<&str>| {
            let mut children = vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_ATTENDEE_NAME, email),
                WbxmlElement::text(PAGE_CALENDAR, CAL_ATTENDEE_EMAIL, email),
            ];
            if let Some(s) = status {
                children.push(WbxmlElement::text(PAGE_CALENDAR, CAL_ATTENDEE_STATUS, s));
            }
            WbxmlElement::container(PAGE_CALENDAR, CAL_ATTENDEE, children)
        };
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_ATTENDEES,
                vec![
                    attendee("a@example.com", Some("0")),
                    attendee("b@example.com", Some("2")),
                    attendee("c@example.com", Some("3")),
                    attendee("d@example.com", Some("4")),
                    attendee("e@example.com", Some("5")),
                    attendee("f@example.com", Some("9")), // unknown: kept raw
                    attendee("g@example.com", None),      // absent
                    attendee("h@example.com", Some("xyz")), // non-numeric
                    // AttendeeType (0x2A, [MS-ASCAL] §2.2.2.6: 1=Required
                    // 2=Optional 3=Resource) — not modeled by the v1 struct.
                    WbxmlElement::container(
                        PAGE_CALENDAR,
                        CAL_ATTENDEE,
                        vec![
                            WbxmlElement::text(PAGE_CALENDAR, CAL_ATTENDEE_NAME, "Room 1"),
                            WbxmlElement::text(
                                PAGE_CALENDAR,
                                CAL_ATTENDEE_EMAIL,
                                "room1@example.com",
                            ),
                            WbxmlElement::text(PAGE_CALENDAR, 0x2A, "3"),
                        ],
                    ),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        let statuses: Vec<Option<u8>> = props.attendees.iter().map(|a| a.status).collect();
        assert_eq!(
            statuses,
            vec![
                Some(0),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(9), // unknown kept raw
                None,    // absent
                None,    // non-numeric
                None,    // AttendeeType-only attendee has no status
            ]
        );
        assert_eq!(props.attendees.len(), 9);
        assert_eq!(props.attendees[0].email, "a@example.com");
        assert_eq!(props.attendees[8].name.as_deref(), Some("Room 1"));
    }

    // ====================================================================
    // M8 Task 3 — Timezone (TZI blob) decoding
    // ====================================================================

    /// Case (a): flat zone — bias -480 (UTC+8), zeroed SYSTEMTIMEs mean no
    /// DST, so both rules decode to None.
    #[test]
    fn parse_timezone_flat_zone_no_dst() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(
                PAGE_CALENDAR,
                CAL_TIMEZONE,
                TZI_FLAT_UTC8,
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(
            props.time_zone,
            Some(TimeZoneBlob {
                raw_base64: Some(TZI_FLAT_UTC8.to_string()),
                parsed: Some(TziTimeZone {
                    base_bias_minutes: -480,
                    standard: None,
                    daylight: None,
                }),
            })
        );
    }

    /// Case (b): DST zone — bias -60 (UTC+1); standard transition last
    /// Sunday of October at 03:00 (offset 0), daylight transition last
    /// Sunday of March at 02:00 (offset -60 ⇒ UTC+2 while in DST).
    #[test]
    fn parse_timezone_dst_zone_last_sunday_transitions() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(PAGE_CALENDAR, CAL_TIMEZONE, TZI_DST_CET)],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        let blob = props.time_zone.expect("Timezone present");
        assert_eq!(blob.raw_base64.as_deref(), Some(TZI_DST_CET));
        assert_eq!(
            blob.parsed,
            Some(TziTimeZone {
                base_bias_minutes: -60,
                standard: Some(TziRule {
                    bias_offset_minutes: 0,
                    month: 10,
                    day_of_week: 0,
                    day_occurrence: 5,
                    hour: 3,
                    minute: 0,
                }),
                daylight: Some(TziRule {
                    bias_offset_minutes: -60,
                    month: 3,
                    day_of_week: 0,
                    day_occurrence: 5,
                    hour: 2,
                    minute: 0,
                }),
            })
        );
    }

    /// Case (c): garbage and short blobs — `parsed` degrades to None with a
    /// warning, the raw string is kept, nothing panics.
    #[test]
    fn parse_timezone_malformed_blob_keeps_raw_none_parsed() {
        for bad in [
            "!!!not-base64!!!", // invalid base64 alphabet
            "3gclAC4AAAA=",     // valid base64, only 8 bytes
            "AQIDBAUGBwgJCg==", // valid base64, 10 bytes
        ] {
            let app_data = WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::text(PAGE_CALENDAR, CAL_TIMEZONE, bad)],
            );
            let props = parse_calendar_application_data(&app_data).expect("parse ok");
            assert_eq!(
                props.time_zone,
                Some(TimeZoneBlob {
                    raw_base64: Some(bad.to_string()),
                    parsed: None,
                }),
                "malformed blob \"{bad}\" must keep raw + parsed None"
            );
        }
    }
}
