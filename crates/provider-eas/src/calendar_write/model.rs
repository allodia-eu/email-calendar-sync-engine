// SPDX-License-Identifier: MPL-2.0
// Write model + pre-flight validation ([MS-ASCAL] §2.2).

use serde::{Deserialize, Serialize};

use crate::calendar::{CalendarAttendee, CalendarRecurrence, is_valid_eas_datetime};

// ============================================================================
// Write model
// ============================================================================

/// One Calendar item to upload — the write-direction twin of
/// `calendar::CalendarEventProps` ([MS-ASCAL] §2.2). Tasks 2-3 embed this
/// in Sync Add/Change Commands; it never talks to the network itself.
///
/// DateTime fields carry the wire string verbatim — Compact DateTime per
/// [MS-ASDTYPE] §2.7.2, e.g. `20260818T090000Z` (all-day items use UTC
/// midnight, [MS-ASCAL] §2.2.2.1). [`CalendarEventWrite::validate`] checks
/// the shape before upload; the builder itself is infallible (the email
/// `build_sync_change_request` precedent).
///
/// `attendees`/`recurrence` REUSE the parse-model types so a downsynced
/// item can round-trip: [`CalendarAttendee`] (`AttendeeStatus` is skipped
/// on write — server-owned) and
/// [`CalendarRecurrence`] (`no_end` is
/// derived, never a wire token).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarEventWrite {
    /// `StartTime` — required ([MS-ASCAL] §2.2.2.42), Compact DateTime.
    pub start_time: String,
    /// `EndTime` — required ([MS-ASCAL] §2.2.2.20), Compact DateTime.
    pub end_time: String,
    /// `AllDayEvent` — required ([MS-ASCAL] §2.2.2.1): `"1"`/`"0"`.
    pub all_day_event: bool,
    /// `Timezone` — required: the base64 [MS-ASDTYPE] §2.7.6 TZI blob
    /// (build one with
    /// [`build_fixed_offset_tzi_base64`]).
    pub time_zone_base64: String,
    /// `Subject` ([MS-ASCAL] §2.2.2.43) — omitted from the wire when `None`.
    pub subject: Option<String>,
    /// `Location` ([MS-ASCAL] §2.2.2.27) — the ≤14.1 calendar-page leaf
    /// form is written (v1 targets the same shape the parse accepts).
    pub location: Option<String>,
    /// Plain-text body — emitted as `airsyncbase:Body` with `Type = "1"`
    /// ([MS-ASWBXML] §2.1.2.1.5 note 1; [MS-ASAIRS] §2.2.2.10/§2.2.2.15).
    pub body_plain: Option<String>,
    /// `OrganizerEmail` ([MS-ASCAL] §2.2.2.35).
    pub organizer_email: Option<String>,
    /// `OrganizerName` ([MS-ASCAL] §2.2.2.36).
    pub organizer_name: Option<String>,
    /// `Sensitivity` — 0=Normal 1=Personal 2=Private 3=Confidential
    /// ([MS-ASCAL] §2.2.2.41).
    pub sensitivity: Option<u8>,
    /// `BusyStatus` — 0=Free 1=Tentative 2=Busy 3=Out of Office
    /// 4=Working elsewhere ([MS-ASCAL] §2.2.2.9, v20220429).
    pub busy_status: Option<u8>,
    /// `Reminder` minutes before start ([MS-ASCAL] §2.2.2.38).
    pub reminder_minutes: Option<u32>,
    /// `Attendees` — emitted as `Attendees > Attendee { Email, Name? }`
    /// ([MS-ASCAL] §2.2.2.4/§2.2.2.3); the container is omitted when empty.
    pub attendees: Vec<CalendarAttendee>,
    /// `Recurrence` pattern ([MS-ASCAL] §2.2.2.37) — omitted when `None`.
    pub recurrence: Option<CalendarRecurrence>,
}

/// Validation failure for [`CalendarEventWrite::validate`] — one variant
/// per reject path (the `thiserror` style of the crate's other small
/// enums, cf. `autodiscover::AutoDiscoverError`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CalendarWriteError {
    /// `start_time` is not a datetime in a shape the wire accepts.
    #[error(
        "invalid StartTime {0:?}: expected Compact DateTime yyyyMMdd'T'HHmmss'Z' \
         ([MS-ASDTYPE] §2.7.2)"
    )]
    InvalidStartTime(String),
    /// `end_time` is not a datetime in a shape the wire accepts.
    #[error(
        "invalid EndTime {0:?}: expected Compact DateTime yyyyMMdd'T'HHmmss'Z' \
         ([MS-ASDTYPE] §2.7.2)"
    )]
    InvalidEndTime(String),
    /// `time_zone_base64` is empty — the server requires a TZI blob.
    #[error("time_zone_base64 must not be empty ([MS-ASDTYPE] §2.7.6 TimeZone blob)")]
    EmptyTimeZone,
    /// `sensitivity` outside the [MS-ASCAL] §2.2.2.41 enum (0..=3).
    #[error("Sensitivity {0} outside 0..=3 ([MS-ASCAL] §2.2.2.41)")]
    SensitivityOutOfRange(u8),
    /// `busy_status` outside the [MS-ASCAL] §2.2.2.9 enum (0..=4).
    #[error("BusyStatus {0} outside 0..=4 ([MS-ASCAL] §2.2.2.9)")]
    BusyStatusOutOfRange(u8),
}

impl CalendarEventWrite {
    /// Pre-flight check before [`build_calendar_application_data`].
    ///
    /// The builder itself is infallible (the email write precedent), so a
    /// caller that skips this simply ships what it set — this method is the
    /// loud, early reject for the shapes the server would bounce:
    /// * `start_time`/`end_time` must pass the shared datetime validation
    ///   (`calendar::is_valid_eas_datetime` — the spec Compact form; the separated RFC 3339 form is
    ///   accepted defensively, exactly like the parse side, and kept verbatim);
    /// * `time_zone_base64` must be non-empty;
    /// * `sensitivity` ≤ 3 and `busy_status` ≤ 4 when set.
    ///
    /// # Errors
    ///
    /// Returns the first failed rule as a `CalendarWriteError` (datetime shape,
    /// empty TimeZone blob, out-of-range enum, …).
    pub fn validate(&self) -> Result<(), CalendarWriteError> {
        if !is_valid_eas_datetime(&self.start_time) {
            return Err(CalendarWriteError::InvalidStartTime(
                self.start_time.clone(),
            ));
        }
        if !is_valid_eas_datetime(&self.end_time) {
            return Err(CalendarWriteError::InvalidEndTime(self.end_time.clone()));
        }
        if self.time_zone_base64.is_empty() {
            return Err(CalendarWriteError::EmptyTimeZone);
        }
        if let Some(s) = self.sensitivity
            && s > 3
        {
            return Err(CalendarWriteError::SensitivityOutOfRange(s));
        }
        if let Some(b) = self.busy_status
            && b > 4
        {
            return Err(CalendarWriteError::BusyStatusOutOfRange(b));
        }
        Ok(())
    }
}
