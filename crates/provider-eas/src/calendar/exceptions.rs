// SPDX-License-Identifier: MPL-2.0

use super::{
    BASE_LOCATION, CAL_ALL_DAY_EVENT, CAL_DELETED, CAL_END_TIME, CAL_EXCEPTION,
    CAL_EXCEPTION_START_TIME, CAL_LOCATION, CAL_START_TIME, CAL_SUBJECT, PAGE_CALENDAR,
    fields::{
        parse_bool_field, parse_calendar_body, parse_datetime_field, parse_tri_bool_field,
        tag_label, text_value_opt,
    },
    location::parse_location_16x,
    model::CalendarException,
};
use crate::wbxml::{
    WbxmlElement,
    tags::{base, pages},
};

/// Parse an `Exceptions` container ([MS-ASCAL] §2.2.2.22) — a list of
/// `Exception` children (spec max 256, not enforced).
pub(super) fn parse_exceptions(elem: &WbxmlElement) -> Vec<CalendarException> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{CAL_EXCEPTIONS, CAL_SENSITIVITY, parse_calendar_application_data},
        commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    };

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
}
