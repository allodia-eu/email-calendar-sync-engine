// SPDX-License-Identifier: MPL-2.0
//! parse_application_data: meeting-request fields (MessageClass, MeetingMessageType, MeetingRequest
//! subtree, GlobalObjectId).

use super::*;

// ---- Task 4: meeting request fields (MessageClass / MeetingMessageType /
// MeetingRequest subtree) ----
//
// A meeting invitation arrives as an ordinary Email-class Sync item whose
// ApplicationData carries (wire-verified token layout):
//   * email:MessageClass (2, 0x13) = "IPM.Schedule.Meeting.Request"
//   * email2:MeetingMessageType (22, 0x13) — [MS-ASEMAIL] §2.2.2.47: 0=initial, 1=full
//     update/request, 2=informational update, 3=outdated, 4=delegated copy, 5=exception
//     cancellation, 6=exception reply. [MS-ASCMD] §3.1.5.6: only 1|2 arm the
//     Accept/Tentative/Decline UI.
//   * email:MeetingRequest (2, 0x22) container whose children are ALSO Email-page tokens
//     ([MS-ASEMAIL] §2.2.2.48): AllDayEvent 0x1A, EndTime 0x1E, InstanceType 0x1F, Location 0x21,
//     Organizer 0x23, ResponseRequested 0x26, StartTime 0x31.
// The parser must surface all of it on EasItem so the engine can persist
// it and the reading pane can render the banner + response buttons
// without a refetch.

/// Fixture: a full meeting-invitation ApplicationData (MessageClass +
/// MeetingMessageType=1 + a fully-populated MeetingRequest subtree).
fn fixture_meeting_app_data() -> WbxmlElement {
    use provider_eas::wbxml::tags::{email, email2};
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(email::PAGE, email::SUBJECT, "Weekly sync"),
            WbxmlElement::text(
                email::PAGE,
                email::MESSAGE_CLASS,
                "IPM.Schedule.Meeting.Request",
            ),
            WbxmlElement::text(email2::PAGE, email2::MEETING_MESSAGE_TYPE, "1"),
            WbxmlElement::container(
                email::PAGE,
                email::MEETING_REQUEST,
                vec![
                    WbxmlElement::text(email::PAGE, email::ALL_DAY_EVENT, "0"),
                    WbxmlElement::text(email::PAGE, email::START_TIME, "2026-08-06T09:00:00.000Z"),
                    WbxmlElement::text(email::PAGE, email::END_TIME, "2026-08-06T09:30:00.000Z"),
                    WbxmlElement::text(email::PAGE, email::INSTANCE_TYPE, "0"),
                    WbxmlElement::text(email::PAGE, email::LOCATION, "Boardroom 3"),
                    WbxmlElement::text(email::PAGE, email::ORGANIZER, "boss@example.com"),
                    WbxmlElement::text(email::PAGE, email::RESPONSE_REQUESTED, "1"),
                ],
            ),
        ],
    )
}

/// Full invitation: MessageClass, MeetingMessageType and every
/// MeetingRequest child populate the typed EasItem fields.
#[test]
fn parse_application_data_meeting_request_populates_all_fields() {
    let item = parse_application_data_for_test("5:777", &fixture_meeting_app_data());
    assert_eq!(item.subject.as_deref(), Some("Weekly sync"));
    assert_eq!(
        item.message_class.as_deref(),
        Some("IPM.Schedule.Meeting.Request"),
        "MessageClass must no longer be discarded by the skip-list"
    );
    assert_eq!(
        item.meeting_message_type,
        Some(1),
        "email2:MeetingMessageType=1 (full update/request) must surface"
    );
    let meeting = item
        .meeting
        .as_ref()
        .expect("MeetingRequest container must yield Some(meeting)");
    assert_eq!(
        meeting.start_time.as_deref(),
        Some("2026-08-06T09:00:00.000Z")
    );
    assert_eq!(
        meeting.end_time.as_deref(),
        Some("2026-08-06T09:30:00.000Z")
    );
    assert_eq!(meeting.location.as_deref(), Some("Boardroom 3"));
    assert_eq!(meeting.organizer.as_deref(), Some("boss@example.com"));
    assert_eq!(meeting.response_requested, Some(true));
    assert_eq!(meeting.all_day_event, Some(false));
    assert_eq!(meeting.instance_type, Some(0));
}

/// Plain mail: MessageClass surfaces, meeting fields stay None.
#[test]
fn parse_application_data_plain_ipm_note_sets_message_class_only() {
    use provider_eas::wbxml::tags::email;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(email::PAGE, email::SUBJECT, "Just mail"),
            WbxmlElement::text(email::PAGE, email::MESSAGE_CLASS, "IPM.Note"),
        ],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.message_class.as_deref(), Some("IPM.Note"));
    assert_eq!(item.meeting_message_type, None);
    assert!(
        item.meeting.is_none(),
        "no MeetingRequest element → meeting must stay None"
    );
}

/// A MeetingRequest with only SOME children sets only those fields; the
/// rest stay None (servers omit optional properties). The container's
/// mere presence still yields `Some` so the UI can tell "meeting item"
/// apart from "not a meeting".
#[test]
fn parse_application_data_meeting_request_partial_children() {
    use provider_eas::wbxml::tags::email;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(
                email::PAGE,
                email::MESSAGE_CLASS,
                "IPM.Schedule.Meeting.Request",
            ),
            WbxmlElement::container(
                email::PAGE,
                email::MEETING_REQUEST,
                vec![
                    WbxmlElement::text(email::PAGE, email::START_TIME, "2026-08-07T14:00:00.000Z"),
                    WbxmlElement::text(email::PAGE, email::LOCATION, "Teams"),
                ],
            ),
        ],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    let meeting = item
        .meeting
        .as_ref()
        .expect("present MeetingRequest must yield Some even when sparse");
    assert_eq!(
        meeting.start_time.as_deref(),
        Some("2026-08-07T14:00:00.000Z")
    );
    assert_eq!(meeting.location.as_deref(), Some("Teams"));
    assert_eq!(meeting.end_time, None);
    assert_eq!(meeting.organizer, None);
    assert_eq!(meeting.response_requested, None);
    assert_eq!(meeting.all_day_event, None);
    assert_eq!(meeting.instance_type, None);
}

// ====================================================================
// M8 follow-up #4 — the MeetingRequest's calendar-identity key, the
// EXACT-KEY invite↔event correlation ([MS-ASEMAIL] §3.1.4.7): at 16.0/16.1
// the MeetingRequest carries `calendar:UID` (page 4, 0x28) verbatim — "no
// conversion is necessary" ([MS-ASWBXML] §2.1.2.1.4 note 4); at ≤14.1 it
// carries `email:GlobalObjId` (page 2, 0x34, base64), which §3.1.4.7
// steps 1-5 convert to the same UID string space the calendar item's
// calendar:UID lives in.
// ====================================================================

/// 16.x form: the calendar:UID child of MeetingRequest surfaces verbatim —
/// byte-identical to the calendar item's UID, the exact-key join value.
#[test]
fn parse_meeting_request_uid_16x_verbatim() {
    use provider_eas::{
        calendar::{CAL_UID, PAGE_CALENDAR},
        wbxml::tags::email,
    };
    const GO: &str = "040000008200E00074C5B7101A82E00800000000E040C9C12685C401";
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(
                email::PAGE,
                email::MESSAGE_CLASS,
                "IPM.Schedule.Meeting.Request",
            ),
            WbxmlElement::container(
                email::PAGE,
                email::MEETING_REQUEST,
                vec![
                    WbxmlElement::text(email::PAGE, email::START_TIME, "2026-08-18T09:00:00.000Z"),
                    // The 16.x wire form: the CALENDAR-page UID tag inside
                    // the Email-page MeetingRequest container.
                    WbxmlElement::text(PAGE_CALENDAR, CAL_UID, GO),
                ],
            ),
        ],
    );
    let item = parse_application_data_for_test("5:777", &app_data);
    let meeting = item.meeting.as_ref().expect("MeetingRequest present");
    assert_eq!(
        meeting.uid.as_deref(),
        Some(GO),
        "the 16.x calendar:UID must surface verbatim — the exact-key join value"
    );
}

/// ≤14.1 form, OutlookID ([MS-ASEMAIL] §4.3 example 1): base64 decode →
/// zero bytes 17-20 → hex encode. The golden expected value is the spec's
/// own output.
#[test]
fn parse_meeting_request_global_obj_id_converted_outlook_id() {
    use provider_eas::wbxml::tags::email;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            email::PAGE,
            email::MEETING_REQUEST,
            vec![WbxmlElement::text(
                email::PAGE,
                email::GLOBAL_OBJ_ID,
                "BAAAAIIA4AB0xbcQGoLgCAfUCRDgQMnBJoXEAQAAAAAAAAAAEAAAAAvw7UtuTulOnjnjhns3jvM=",
            )],
        )],
    );
    let item = parse_application_data_for_test("5:778", &app_data);
    let meeting = item.meeting.as_ref().expect("MeetingRequest present");
    assert_eq!(
        meeting.uid.as_deref(),
        Some(
            "040000008200E00074C5B7101A82E00800000000E040C9C12685C4010000000000000000100000000BF0ED4B6E4EE94E9E39E3867B378EF3"
        ),
        "GlobalObjId (OutlookID) must convert per [MS-ASEMAIL] §3.1.4.7: \
         bytes 17-20 zeroed, whole value hex-encoded (spec §4.3 example 1)"
    );
}

/// ≤14.1 form, vCal-Uid ([MS-ASEMAIL] §4.3 example 2): the UID is the
/// embedded vCal UID string, extracted per the §3.1.4.7 length arithmetic.
#[test]
fn parse_meeting_request_global_obj_id_converted_vcal_uid() {
    use provider_eas::wbxml::tags::email;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            email::PAGE,
            email::MEETING_REQUEST,
            vec![WbxmlElement::text(
                email::PAGE,
                email::GLOBAL_OBJ_ID,
                "BAAAAIIA4AB0xbcQGoLgCAAAAAAAAAAAAAAAAAAAAAAAAAAAMwAAAHZDYWwtVWlkAQAAAHs4MTQxMkQzQy0yQTI0LTRFOUQtQjIwRS0xMUY3QkJFOTI3OTl9AA==",
            )],
        )],
    );
    let item = parse_application_data_for_test("5:779", &app_data);
    let meeting = item.meeting.as_ref().expect("MeetingRequest present");
    assert_eq!(
        meeting.uid.as_deref(),
        Some("{81412D3C-2A24-4E9D-B20E-11F7BBE92799}"),
        "GlobalObjId (vCal-Uid) must extract the embedded UID string \
         (spec §4.3 example 2)"
    );
}

/// A malformed GlobalObjId (not base64) degrades to None with a warning —
/// never a panic, never an invented join key.
#[test]
fn parse_meeting_request_malformed_global_obj_id_yields_none_uid() {
    use provider_eas::wbxml::tags::email;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            email::PAGE,
            email::MEETING_REQUEST,
            vec![WbxmlElement::text(
                email::PAGE,
                email::GLOBAL_OBJ_ID,
                "!!!not-base64!!!",
            )],
        )],
    );
    let item = parse_application_data_for_test("5:780", &app_data);
    let meeting = item.meeting.as_ref().expect("MeetingRequest present");
    assert_eq!(meeting.uid, None, "malformed base64 → no join key");
}

// ====================================================================
// M8-L1 variant (2026-08-17 live seed drill) — on real Exchange 16.x the
// meeting-request Location arrives as the page-17 `airsyncbase:Location`
// (0x20) CONTAINER ([MS-ASAIRS] §2.2.2.28), whose text lives in the
// DisplayName child (§2.2.2.22.3) — the identical shape the Calendar
// parse lost LOCATION to (M8-L1, fixed via `calendar::parse_location_16x`).
// Tag-name dispatch here funnels that container into the same
// `text_value_opt`, which reads only the container's own (empty) value.
// ====================================================================

/// RED: a page-17 Location CONTAINER with a DisplayName child must yield
/// the DisplayName text for the meeting info.
#[test]
fn parse_application_data_meeting_request_location_16x_container_reads_display_name_child() {
    use provider_eas::wbxml::tags::{base, email, pages};
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(
                email::PAGE,
                email::MESSAGE_CLASS,
                "IPM.Schedule.Meeting.Request",
            ),
            WbxmlElement::container(
                email::PAGE,
                email::MEETING_REQUEST,
                vec![
                    WbxmlElement::text(email::PAGE, email::START_TIME, "2026-08-19T09:00:00.000Z"),
                    WbxmlElement::container(
                        pages::BASE,
                        base::LOCATION,
                        vec![
                            WbxmlElement::text(
                                pages::BASE,
                                base::DISPLAY_NAME,
                                "Teams Room 4A, Building 2",
                            ),
                            // Structured sibling (§2.2.2.28: all children
                            // optional) — unmodeled in v1, must be skipped
                            // without error. LocationUri = page 17, 0x2C.
                            WbxmlElement::text(pages::BASE, 0x2C, "https://maps.example.com/4a"),
                        ],
                    ),
                ],
            ),
        ],
    );
    let item = parse_application_data_for_test("5:16x", &app_data);
    let meeting = item
        .meeting
        .as_ref()
        .expect("MeetingRequest container must yield Some(meeting)");
    assert_eq!(
        meeting.location.as_deref(),
        Some("Teams Room 4A, Building 2"),
        "airsyncbase:Location container must yield its DisplayName child text"
    );
    // Sibling children are unaffected by the container form.
    assert_eq!(
        meeting.start_time.as_deref(),
        Some("2026-08-19T09:00:00.000Z")
    );
}

/// Task 6 regression guard (NOT RED-driven — pins existing fallback
/// behavior): a malformed (non-numeric) `email2:MeetingMessageType` must
/// still degrade to `None` so the reading pane treats the item as a
/// non-respondable meeting message instead of aborting the item parse.
/// The warn-log added at the parse site is a side effect; only the
/// resulting value is pinned here.
#[test]
fn parse_application_data_meeting_message_type_malformed_yields_none() {
    use provider_eas::wbxml::tags::{email, email2};
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(
                email::PAGE,
                email::MESSAGE_CLASS,
                "IPM.Schedule.Meeting.Request",
            ),
            WbxmlElement::text(email2::PAGE, email2::MEETING_MESSAGE_TYPE, "not-a-number"),
        ],
    );
    let item = parse_application_data_for_test("6:malformed-mmt", &app_data);
    assert_eq!(
        item.meeting_message_type, None,
        "non-numeric MeetingMessageType must fall back to None, never abort"
    );
    assert_eq!(
        item.message_class.as_deref(),
        Some("IPM.Schedule.Meeting.Request"),
        "the rest of the item must still parse around the malformed value"
    );
}
