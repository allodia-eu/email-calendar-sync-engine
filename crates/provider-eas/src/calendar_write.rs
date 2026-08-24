// SPDX-License-Identifier: MPL-2.0
//! Write-direction Calendar `ApplicationData` serialization ([MS-ASCAL]
//! §2.2) — the upload twin of `calendar.rs`'s downsync parse. M8 calendar
//! upsync Task 1; Tasks 2-3 wrap this into Sync Add/Change Commands.
//!
//! Token fidelity red line: every page-4 token below is REUSED from
//! `calendar.rs` (whose values were verified against
//! `docs/Exchange/MS-ASWBXML.txt` §2.1.2.1.5) — no token value is invented
//! here. Element value semantics per [MS-ASCAL] §2.2.2 and [MS-ASDTYPE]
//! (§2.1 boolean `"0"`/`"1"`, §2.7.2 Compact DateTime, §2.7.6 TimeZone).
//!
//! Canonical emission order (fixed, asserted by tests):
//! ```text
//! Timezone, AllDayEvent, StartTime, EndTime (always emitted),
//! Subject?, Location?, Body?, Sensitivity?, BusyStatus?, Reminder?,
//! Attendees?, Recurrence?
//! ```
//! `Option` fields are emitted only when `Some`; the `Attendees` container
//! is omitted when the list is empty.
//!
//! Server-managed on write — NEVER emitted ([MS-ASCAL] §2.2.2): `UID`
//! (§2.2.2.46), `DtStamp` (§2.2.2.18), `MeetingStatus` (§2.2.2.28),
//! `ResponseRequested` (§2.2.2.39), attendee `AttendeeStatus` (§2.2.2.5),
//! and `OrganizerEmail`/`OrganizerName` (§2.2.2.35/§2.2.2.36 — the
//! organizer is derived server-side from the mailbox owner; live probe
//! 2026-08-22: identical Adds WITH organizer fields are rejected with
//! per-item Status 6, without them accepted). The `CalendarEventWrite`
//! organizer fields remain for round-trip bookkeeping; the serializer
//! ignores them.
//!
//! Recurrence: `Recurrence { Type, Interval?, DayOfWeek?, DayOfMonth?,
//! WeekOfMonth?, MonthOfYear?, Until? XOR Occurrences? }` reusing the
//! parse-model [`CalendarRecurrence`]. `no_end` is DERIVED, not a wire
//! token ([MS-ASCAL] §2.2.2.37.1) — never emitted; `Until` wins when both
//! end conditions are (invalidly) set, with a warning.
//!
//! Timezone: fixed-offset TZI blob only (design D6 — no DST rules on
//! write); see [`build_fixed_offset_tzi_base64`].

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};

use crate::{
    calendar::{
        CAL_ALL_DAY_EVENT, CAL_ATTENDEE, CAL_ATTENDEE_EMAIL, CAL_ATTENDEE_NAME, CAL_ATTENDEES,
        CAL_BUSY_STATUS, CAL_END_TIME, CAL_LOCATION, CAL_RECURRENCE, CAL_RECURRENCE_DAY_OF_MONTH,
        CAL_RECURRENCE_DAY_OF_WEEK, CAL_RECURRENCE_INTERVAL, CAL_RECURRENCE_MONTH_OF_YEAR,
        CAL_RECURRENCE_OCCURRENCES, CAL_RECURRENCE_TYPE, CAL_RECURRENCE_UNTIL,
        CAL_RECURRENCE_WEEK_OF_MONTH, CAL_REMINDER, CAL_SENSITIVITY, CAL_START_TIME, CAL_SUBJECT,
        CAL_TIMEZONE, CalendarAttendee, CalendarRecurrence, PAGE_CALENDAR, is_valid_eas_datetime,
    },
    commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    wbxml::{
        WbxmlElement,
        tags::{base, pages},
    },
};

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
/// on write — server-owned) and [`CalendarRecurrence`] (`no_end` is
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
    /// (build one with [`build_fixed_offset_tzi_base64`]).
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
        if let Some(s) = self.sensitivity {
            if s > 3 {
                return Err(CalendarWriteError::SensitivityOutOfRange(s));
            }
        }
        if let Some(b) = self.busy_status {
            if b > 4 {
                return Err(CalendarWriteError::BusyStatusOutOfRange(b));
            }
        }
        Ok(())
    }
}

// ============================================================================
// TZI synthesis (design D6: fixed offset, no DST rules)
// ============================================================================

/// Byte length of the [MS-ASDTYPE] §2.7.6 TimeZone structure — mirrors
/// `calendar::TZI_BLOB_LEN` (kept private there):
/// Bias(4) + StandardName(64) + StandardDate(16) + StandardBias(4) +
/// DaylightName(64) + DaylightDate(16) + DaylightBias(4) = 172.
const TZI_BLOB_LEN: usize = 172;

/// Synthesize a flat fixed-offset TZI blob ([MS-ASDTYPE] §2.7.6), base64
/// STANDARD: `Bias = -(local_minus_utc_minutes)` (§2.7.6 sign convention —
/// UTC−local, so UTC+8 ⇒ Bias −480) with both name fields, both SYSTEMTIME
/// transition rules and both rule biases zeroed = no DST (design D6).
///
/// Round-trips through `calendar::parse_tzi_blob` into a `TziTimeZone`
/// with `standard: None, daylight: None`.
pub fn build_fixed_offset_tzi_base64(local_minus_utc_minutes: i32) -> String {
    // All-zero 172-byte structure, Bias at offset 0 ([MS-ASDTYPE] §2.7.6):
    // zeroed SYSTEMTIMEs = no transitions, zeroed rule biases = no DST.
    // `saturating_neg` so a nonsensical i32::MIN input degrades to i32::MAX
    // instead of panicking on negation overflow (debug builds).
    let mut blob = [0u8; TZI_BLOB_LEN];
    let bias = local_minus_utc_minutes.saturating_neg();
    blob[0..4].copy_from_slice(&bias.to_le_bytes());
    BASE64_STANDARD.encode(blob)
}

// ============================================================================
// ApplicationData serialization
// ============================================================================

/// True when the negotiated protocol version is 16.0 or newer — the
/// [MS-ASWBXML] §2.1.2.1.5 note-2 wire generation where
/// `airsyncbase:Location` replaces the calendar-page Location leaf.
/// Unparseable/absent → true (modern shape; the parse twin accepts both).
fn version_at_least_16(protocol_version: &str) -> bool {
    let major = protocol_version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(16);
    major >= 16
}

/// Build the Calendar-class `ApplicationData` element (page 0, 0x1D) for a
/// Sync Add/Change — the write twin of
/// `calendar::parse_calendar_application_data`.
///
/// `protocol_version` is the negotiated EAS version (e.g. "16.1"): 16.0+
/// servers replace `calendar:Location` with the `airsyncbase:Location`
/// CONTAINER whose value is its `DisplayName` child ([MS-ASWBXML]
/// §2.1.2.1.5 note 2) — live evidence 2026-08-22: an Exchange 16.1 server
/// answers the legacy ≤14.1 page-4 leaf with per-item Status 6 (conversion
/// error, Permanent). Unparseable/absent versions are treated as 16.x (the
/// modern shape; the parse twin accepts BOTH forms either way).
///
/// Infallible (the email `build_sync_change_request` precedent): callers
/// run [`CalendarEventWrite::validate`] first. Children follow the
/// canonical order documented in the module header; see the header also
/// for the server-managed elements that are deliberately NOT emitted.
pub fn build_calendar_application_data(
    w: &CalendarEventWrite,
    protocol_version: &str,
) -> WbxmlElement {
    let location_16x = version_at_least_16(protocol_version);
    let mut children = Vec::with_capacity(14);
    // Canonical order per the module header; the first four are always
    // emitted (schema-required), the rest only when Some / non-empty.
    children.push(WbxmlElement::text(
        PAGE_CALENDAR,
        CAL_TIMEZONE,
        w.time_zone_base64.clone(),
    ));
    children.push(WbxmlElement::text(
        PAGE_CALENDAR,
        CAL_ALL_DAY_EVENT,
        if w.all_day_event { "1" } else { "0" },
    ));
    children.push(WbxmlElement::text(
        PAGE_CALENDAR,
        CAL_START_TIME,
        w.start_time.clone(),
    ));
    children.push(WbxmlElement::text(
        PAGE_CALENDAR,
        CAL_END_TIME,
        w.end_time.clone(),
    ));
    if let Some(subject) = &w.subject {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_SUBJECT,
            subject.clone(),
        ));
    }
    if let Some(location) = &w.location {
        if location_16x {
            // [MS-ASWBXML] §2.1.2.1.5 note 2: 16.0/16.1 replace the
            // calendar-page Location leaf with the AirSyncBase container —
            // the value is its DisplayName child (the M8-L1 downsync shape).
            children.push(WbxmlElement::container(
                crate::wbxml::tags::pages::BASE,
                crate::wbxml::tags::base::LOCATION,
                vec![WbxmlElement::text(
                    crate::wbxml::tags::pages::BASE,
                    crate::wbxml::tags::base::DISPLAY_NAME,
                    location.clone(),
                )],
            ));
        } else {
            children.push(WbxmlElement::text(
                PAGE_CALENDAR,
                CAL_LOCATION,
                location.clone(),
            ));
        }
    }
    if let Some(body) = &w.body_plain {
        children.push(build_body_plain(body));
    }
    // OrganizerEmail/OrganizerName are deliberately NOT emitted: the
    // organizer is derived server-side from the mailbox owner, and a
    // client-supplied organizer is a per-item Status 6 (conversion error)
    // on live Exchange 16.1 — probe evidence 2026-08-22 (identical Adds
    // with and without the organizer fields: rejected vs accepted). The
    // CalendarEventWrite fields stay for round-trip bookkeeping; see the
    // module header's server-managed list.
    if let Some(sensitivity) = w.sensitivity {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_SENSITIVITY,
            sensitivity.to_string(),
        ));
    }
    if let Some(busy_status) = w.busy_status {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_BUSY_STATUS,
            busy_status.to_string(),
        ));
    }
    if let Some(minutes) = w.reminder_minutes {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_REMINDER,
            minutes.to_string(),
        ));
    }
    if !w.attendees.is_empty() {
        children.push(build_attendees(&w.attendees));
    }
    if let Some(recurrence) = &w.recurrence {
        children.push(build_recurrence(recurrence));
    }
    WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, children)
}

/// `airsyncbase:Body` container with `Type = "1"` (PlainText) — the 12.0+
/// calendar body form ([MS-ASWBXML] §2.1.2.1.5 note 1; [MS-ASAIRS]
/// §2.2.2.10/§2.2.2.15), the same shape `calendar::parse_calendar_body`
/// reads back.
fn build_body_plain(data: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::BASE,
        base::BODY,
        vec![
            WbxmlElement::text(pages::BASE, base::TYPE, "1"),
            WbxmlElement::text(pages::BASE, base::DATA, data),
        ],
    )
}

/// `Attendees > Attendee { Email, Name? }` ([MS-ASCAL] §2.2.2.4/§2.2.2.3/
/// §2.2.2.19/§2.2.2.30). `AttendeeStatus` (§2.2.2.5) is deliberately NOT
/// written — server-owned on upload.
fn build_attendees(attendees: &[CalendarAttendee]) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_CALENDAR,
        CAL_ATTENDEES,
        attendees
            .iter()
            .map(|a| {
                let mut children = vec![WbxmlElement::text(
                    PAGE_CALENDAR,
                    CAL_ATTENDEE_EMAIL,
                    a.email.clone(),
                )];
                if let Some(name) = &a.name {
                    children.push(WbxmlElement::text(
                        PAGE_CALENDAR,
                        CAL_ATTENDEE_NAME,
                        name.clone(),
                    ));
                }
                WbxmlElement::container(PAGE_CALENDAR, CAL_ATTENDEE, children)
            })
            .collect(),
    )
}

/// `Recurrence` container ([MS-ASCAL] §2.2.2.37) in the canonical child
/// order Type, Interval?, DayOfWeek?, DayOfMonth?, WeekOfMonth?,
/// MonthOfYear?, then `Until` XOR `Occurrences` (§2.2.2.47 — mutually
/// exclusive; Until wins when the struct invalidly carries both, with a
/// warning). `no_end` is derived, never a wire token (§2.2.2.37.1).
fn build_recurrence(rec: &CalendarRecurrence) -> WbxmlElement {
    if rec.until.is_some() && rec.occurrences.is_some() {
        log::warn!(
            "calendar Recurrence write: both Until and Occurrences set — mutually \
             exclusive per [MS-ASCAL] §2.2.2.47; writing Until only"
        );
    }
    let mut children = vec![WbxmlElement::text(
        PAGE_CALENDAR,
        CAL_RECURRENCE_TYPE,
        rec.recurrence_type.to_string(),
    )];
    if let Some(v) = rec.interval {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_INTERVAL,
            v.to_string(),
        ));
    }
    if let Some(v) = rec.day_of_week {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_DAY_OF_WEEK,
            v.to_string(),
        ));
    }
    if let Some(v) = rec.day_of_month {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_DAY_OF_MONTH,
            v.to_string(),
        ));
    }
    if let Some(v) = rec.week_of_month {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_WEEK_OF_MONTH,
            v.to_string(),
        ));
    }
    if let Some(v) = rec.month_of_year {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_MONTH_OF_YEAR,
            v.to_string(),
        ));
    }
    if let Some(until) = &rec.until {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_UNTIL,
            until.clone(),
        ));
    } else if let Some(occurrences) = rec.occurrences {
        children.push(WbxmlElement::text(
            PAGE_CALENDAR,
            CAL_RECURRENCE_OCCURRENCES,
            occurrences.to_string(),
        ));
    }
    WbxmlElement::container(PAGE_CALENDAR, CAL_RECURRENCE, children)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{
            CAL_ORGANIZER_EMAIL, CAL_ORGANIZER_NAME, TimeZoneBlob, TziTimeZone,
            parse_calendar_application_data, parse_tzi_blob, tests::TZI_FLAT_UTC8,
        },
        wbxml::{WbxmlValue, deserialize_to_tree, serialize_tree},
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

    /// UTC+8 (local_minus_utc = 480 ⇒ Bias −480) must byte-for-byte equal
    /// the golden flat fixture from `calendar.rs` — one source of truth for
    /// the 172-byte layout — and decode to a rule-less TziTimeZone.
    #[test]
    fn tzi_utc8_matches_golden_flat_fixture() {
        let b64 = build_fixed_offset_tzi_base64(480);
        assert_eq!(
            b64, TZI_FLAT_UTC8,
            "synthesized blob must equal the golden flat UTC+8 fixture"
        );
        assert_eq!(
            BASE64_STANDARD.decode(&b64).expect("valid base64").len(),
            172
        );
        assert_eq!(
            parse_tzi_blob(&b64),
            Some(TziTimeZone {
                base_bias_minutes: -480,
                standard: None,
                daylight: None,
            })
        );
    }

    /// Bias sign convention at the extremes of the ruling: UTC+0 ⇒ Bias 0;
    /// UTC−5 (local_minus_utc = −300) ⇒ Bias +300. All rules stay inactive.
    #[test]
    fn tzi_zero_and_negative_offsets() {
        for (local_minus_utc, expected_bias) in [(0, 0), (-300, 300)] {
            let b64 = build_fixed_offset_tzi_base64(local_minus_utc);
            assert_eq!(
                BASE64_STANDARD.decode(&b64).expect("valid base64").len(),
                172,
                "local_minus_utc {local_minus_utc}"
            );
            assert_eq!(
                parse_tzi_blob(&b64),
                Some(TziTimeZone {
                    base_bias_minutes: expected_bias,
                    standard: None,
                    daylight: None,
                }),
                "local_minus_utc {local_minus_utc} ⇒ Bias {expected_bias}"
            );
        }
    }

    // ====================================================================
    // Structural serialization
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
}
