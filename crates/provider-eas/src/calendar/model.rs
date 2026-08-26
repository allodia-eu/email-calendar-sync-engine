// SPDX-License-Identifier: MPL-2.0
// Model types ([MS-ASCAL] §2.2)

use serde::{Deserialize, Serialize};

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
