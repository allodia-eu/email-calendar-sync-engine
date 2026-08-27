// SPDX-License-Identifier: MPL-2.0
//! MS-ASCAL Calendar-class item model ([MS-ASCAL] §2.2) + downsync parse of a
//! Calendar-class `ApplicationData` element.
//!
//! Token fidelity red line: every page-4 token value below was looked up in
//! `docs/Exchange/MS-ASWBXML.txt` §2.1.2.1.5 ("Code Page 4: Calendar",
//! v20220429) and cross-checked against the same table in
//! `docs/Exchange/[MS-ASWBXML].pdf` — never from memory. The values match
//! `CALENDAR_TOKENS` in `wbxml/code_pages/pages_00_09.rs`. Element value semantics per
//! [MS-ASCAL] §2.2.2 (`docs/Exchange/[MS-ASCAL].pdf`) and [MS-ASDTYPE]
//! (§2.1 boolean `"0"`/`"1"`, §2.7.2 Compact DateTime, §2.7.6 TimeZone).
//!
//! Downsync only: v1 never BUILDS Calendar-class items for upload.
//!
//! Parse policy (the Email `parse_application_data` precedent in the
//! `commands/sync/` module, `parse_item.rs`): malformed values → `log::warn!`
//! with the element name + offending text, then default — never panic, never
//! swallow silently; tokens this task does not model → `log::debug!` skip.

mod attendees;
mod datetime;
mod exceptions;
mod fields;
mod location;
mod model;
mod parse;
mod recurrence;
mod timezone;

pub(crate) use datetime::is_valid_eas_datetime;
pub(crate) use location::parse_location_16x;
pub use model::{
    CalendarAttendee, CalendarEventProps, CalendarException, CalendarRecurrence, TimeZoneBlob,
    TziRule, TziTimeZone,
};
pub use parse::parse_calendar_application_data;
#[cfg(test)]
pub(crate) use timezone::parse_tzi_blob;

#[cfg(test)]
pub(crate) mod tests;

use crate::wbxml::tags::base;

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
