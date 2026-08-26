// SPDX-License-Identifier: MPL-2.0
// ApplicationData serialization (the wire builder).

use super::model::CalendarEventWrite;
use crate::{
    calendar::{
        CAL_ALL_DAY_EVENT, CAL_ATTENDEE, CAL_ATTENDEE_EMAIL, CAL_ATTENDEE_NAME, CAL_ATTENDEES,
        CAL_BUSY_STATUS, CAL_END_TIME, CAL_LOCATION, CAL_RECURRENCE, CAL_RECURRENCE_DAY_OF_MONTH,
        CAL_RECURRENCE_DAY_OF_WEEK, CAL_RECURRENCE_INTERVAL, CAL_RECURRENCE_MONTH_OF_YEAR,
        CAL_RECURRENCE_OCCURRENCES, CAL_RECURRENCE_TYPE, CAL_RECURRENCE_UNTIL,
        CAL_RECURRENCE_WEEK_OF_MONTH, CAL_REMINDER, CAL_SENSITIVITY, CAL_START_TIME, CAL_SUBJECT,
        CAL_TIMEZONE, CalendarAttendee, CalendarRecurrence, PAGE_CALENDAR,
    },
    commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    wbxml::{
        WbxmlElement,
        tags::{base, pages},
    },
};

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
