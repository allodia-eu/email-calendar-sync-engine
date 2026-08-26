// SPDX-License-Identifier: MPL-2.0

use super::{
    CAL_ATTENDEE, CAL_ATTENDEE_EMAIL, CAL_ATTENDEE_NAME, CAL_ATTENDEE_STATUS, PAGE_CALENDAR,
    fields::{tag_label, text_value_opt},
    model::CalendarAttendee,
};
use crate::wbxml::WbxmlElement;

/// Parse an `Attendees` container ([MS-ASCAL] §2.2.2.4) — a list of
/// `Attendee` children.
pub(super) fn parse_attendees(elem: &WbxmlElement) -> Vec<CalendarAttendee> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{CAL_ATTENDEES, parse_calendar_application_data},
        commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    };

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
}
