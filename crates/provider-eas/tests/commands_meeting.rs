// SPDX-License-Identifier: MPL-2.0
//! MeetingResponse tests: request tree and status parse.
use provider_eas::commands::{tests_common::*, *};

#[test]
fn meeting_response_request_shape_and_child_order() {
    let tree = build_meeting_response_request("5", "9:42", "1", None, true);
    assert_eq!(tree.page, PAGE_MREQ);
    assert_eq!(tree.token, MREQ_MEETING_RESPONSE);
    assert_eq!(tree.children.len(), 1);
    let request = &tree.children[0];
    assert_eq!(request.page, PAGE_MREQ);
    assert_eq!(request.token, MREQ_REQUEST);
    let tokens: Vec<u8> = request.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![
            MREQ_USER_RESPONSE,
            MREQ_COLLECTION_ID,
            MREQ_REQUEST_ID,
            MREQ_SEND_RESPONSE
        ],
        "Request children must serialize as UserResponse, CollectionId, RequestId, SendResponse"
    );
    // Values land on the right elements.
    assert_eq!(
        text_value(&request.children[0]).expect("user response text"),
        "1"
    );
    assert_eq!(
        text_value(&request.children[1]).expect("collection id text"),
        "5"
    );
    assert_eq!(
        text_value(&request.children[2]).expect("request id text"),
        "9:42"
    );
    // SendResponse is an EMPTY element (its mere presence asks a 16.0/16.1
    // server to email the organizer — MS-ASCMD §2.2.1.11).
    assert!(matches!(request.children[3].value, WbxmlValue::Empty));
}

/// `send_response: false` omits the SendResponse element entirely (the
/// token is 16.0/16.1-only per MS-ASWBXML §2.1.2.1.9).
#[test]
fn meeting_response_request_omits_send_response_when_false() {
    let tree = build_meeting_response_request("5", "9:42", "3", None, false);
    let request = &tree.children[0];
    let tokens: Vec<u8> = request.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![MREQ_USER_RESPONSE, MREQ_COLLECTION_ID, MREQ_REQUEST_ID]
    );
    assert_eq!(
        text_value(&request.children[0]).expect("user response text"),
        "3"
    );
}

/// Golden-bytes guard on the wire encoding: page switch to 8, then the
/// MeetingResponse/Request containers, then the child tokens in the
/// confirmed order. SendResponse (empty) is emitted BARE (no WITH_CONTENT
/// bit), like SaveInSentItems in the SendMail tests.
#[test]
fn meeting_response_request_golden_bytes_child_order() {
    // Test values deliberately avoid ASCII bytes that collide with the
    // token values under test (0x46='F', 0x48='H', 0x4C='L', 0x52='R').
    let tree = build_meeting_response_request("5", "9:42", "1", None, true);
    let wbxml = provider_eas::wbxml::serialize_tree(&tree).expect("serialize_tree");

    // Page switch to MeetingResponse (page 8) + root token with content.
    let root_idx = wbxml
        .windows(3)
        .position(|w| w[0] == 0x00 && w[1] == PAGE_MREQ && w[2] == (MREQ_MEETING_RESPONSE | 0x40))
        .expect("SWITCH_PAGE(0x00 0x08) + MeetingResponse|0x40 must be present");
    let after_root = &wbxml[root_idx + 3..];

    let pos = |token: u8, from: usize| -> usize {
        after_root[from..]
            .iter()
            .position(|&b| b == token)
            .map_or_else(
                || panic!("token {token:#04x} missing from wire bytes"),
                |p| p + from,
            )
    };
    let i_request = pos(MREQ_REQUEST | 0x40, 0);
    let i_user_response = pos(MREQ_USER_RESPONSE | 0x40, i_request);
    let i_collection_id = pos(MREQ_COLLECTION_ID | 0x40, i_user_response);
    let i_request_id = pos(MREQ_REQUEST_ID | 0x40, i_collection_id);
    let i_send_response = pos(MREQ_SEND_RESPONSE, i_request_id); // bare empty tag
    assert!(
        i_request < i_user_response
            && i_user_response < i_collection_id
            && i_collection_id < i_request_id
            && i_request_id < i_send_response,
        "wire token order must be Request, UserResponse, CollectionId, RequestId, SendResponse"
    );
}

/// Spec §4.17: responding to ONE instance of a recurring meeting adds
/// `InstanceId` (page 8, 0x0E, 14.1+ per [MS-ASWBXML] §2.1.2.1.9) to the
/// Request. §6.25 child order: UserResponse, CollectionId, RequestId,
/// InstanceId, SendResponse — InstanceId serializes BETWEEN RequestId and
/// SendResponse when both are present.
#[test]
fn meeting_response_request_instance_id_between_request_id_and_send_response() {
    // §6.25 restricts InstanceId to a 24-char [MS-ASCAL] UTC timestamp.
    let tree =
        build_meeting_response_request("5", "9:42", "1", Some("2026-08-05T09:00:00.000Z"), true);
    let request = &tree.children[0];
    let tokens: Vec<u8> = request.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![
            MREQ_USER_RESPONSE,
            MREQ_COLLECTION_ID,
            MREQ_REQUEST_ID,
            MREQ_INSTANCE_ID,
            MREQ_SEND_RESPONSE
        ],
        "Request children must serialize as UserResponse, CollectionId, RequestId, InstanceId, SendResponse"
    );
    assert_eq!(
        text_value(&request.children[3]).expect("instance id text"),
        "2026-08-05T09:00:00.000Z"
    );
    // SendResponse stays an EMPTY element after InstanceId.
    assert!(matches!(request.children[4].value, WbxmlValue::Empty));
}

/// With `send_response: false` (protocol < 16.0), InstanceId is the LAST
/// Request child — no SendResponse follows it.
#[test]
fn meeting_response_request_instance_id_without_send_response() {
    let tree =
        build_meeting_response_request("5", "9:42", "2", Some("2026-08-05T09:00:00.000Z"), false);
    let request = &tree.children[0];
    let tokens: Vec<u8> = request.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![
            MREQ_USER_RESPONSE,
            MREQ_COLLECTION_ID,
            MREQ_REQUEST_ID,
            MREQ_INSTANCE_ID
        ]
    );
    assert_eq!(
        text_value(&request.children[3]).expect("instance id text"),
        "2026-08-05T09:00:00.000Z"
    );
}

/// Golden-bytes guard with an InstanceId: on the wire the InstanceId tag
/// (0x0E | WITH_CONTENT) sits between RequestId's text and the bare
/// SendResponse empty tag.
#[test]
fn meeting_response_request_golden_bytes_with_instance_id() {
    let tree =
        build_meeting_response_request("5", "9:42", "1", Some("2026-08-05T09:00:00.000Z"), true);
    let wbxml = provider_eas::wbxml::serialize_tree(&tree).expect("serialize_tree");

    let root_idx = wbxml
        .windows(3)
        .position(|w| w[0] == 0x00 && w[1] == PAGE_MREQ && w[2] == (MREQ_MEETING_RESPONSE | 0x40))
        .expect("SWITCH_PAGE(0x00 0x08) + MeetingResponse|0x40 must be present");
    let after_root = &wbxml[root_idx + 3..];

    let pos = |token: u8, from: usize| -> usize {
        after_root[from..]
            .iter()
            .position(|&b| b == token)
            .map_or_else(
                || panic!("token {token:#04x} missing from wire bytes"),
                |p| p + from,
            )
    };
    // The instance-id timestamp chars (digits, '-', 'T', ':', '.', 'Z')
    // cannot collide with the tokens searched here (0x4E, 0x12).
    let i_request = pos(MREQ_REQUEST | 0x40, 0);
    let i_user_response = pos(MREQ_USER_RESPONSE | 0x40, i_request);
    let i_collection_id = pos(MREQ_COLLECTION_ID | 0x40, i_user_response);
    let i_request_id = pos(MREQ_REQUEST_ID | 0x40, i_collection_id);
    let i_instance_id = pos(MREQ_INSTANCE_ID | 0x40, i_request_id);
    let i_send_response = pos(MREQ_SEND_RESPONSE, i_instance_id); // bare empty tag
    assert!(
        i_request < i_user_response
            && i_user_response < i_collection_id
            && i_collection_id < i_request_id
            && i_request_id < i_instance_id
            && i_instance_id < i_send_response,
        "wire token order must be Request, UserResponse, CollectionId, RequestId, InstanceId, SendResponse"
    );
}

/// InstanceId round-trips through serialize → parse.
#[test]
fn meeting_response_request_with_instance_id_round_trips() {
    let tree = build_meeting_response_request(
        "col-7",
        "req-11",
        "2",
        Some("2026-08-05T09:00:00.000Z"),
        true,
    );
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

#[test]
fn meeting_response_request_round_trips() {
    let tree = build_meeting_response_request("col-7", "req-11", "2", None, true);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Success shape per MS-ASCMD §6.26: MeetingResponse > Result > Status.
#[test]
fn meeting_response_response_parses_result_status_success() {
    let response = WbxmlElement::container(
        PAGE_MREQ,
        MREQ_MEETING_RESPONSE,
        vec![WbxmlElement::container(
            PAGE_MREQ,
            MREQ_RESULT,
            vec![
                WbxmlElement::text(PAGE_MREQ, MREQ_REQUEST_ID, "9:42"),
                WbxmlElement::text(PAGE_MREQ, MREQ_STATUS, "1"),
                WbxmlElement::text(PAGE_MREQ, MREQ_CALENDAR_ID, "cal-1"),
            ],
        )],
    );
    assert_eq!(
        parse_meeting_response_response(&response).expect("parse"),
        1
    );
}

/// Failure statuses are data (Ok(status)), not parse errors — the client
/// call site surfaces them as EasError::CommandStatus.
#[test]
fn meeting_response_response_parses_error_status() {
    let response = WbxmlElement::container(
        PAGE_MREQ,
        MREQ_MEETING_RESPONSE,
        vec![WbxmlElement::container(
            PAGE_MREQ,
            MREQ_RESULT,
            vec![WbxmlElement::text(PAGE_MREQ, MREQ_STATUS, "2")],
        )],
    );
    assert_eq!(
        parse_meeting_response_response(&response).expect("parse"),
        2
    );
}

/// Defensive fallback: a top-level Status (no Result wrapper) is accepted;
/// an entirely Status-less response defaults to success (1), matching the
/// convention of the other parsers in this file.
#[test]
fn meeting_response_response_top_level_status_fallback() {
    let top_level = WbxmlElement::container(
        PAGE_MREQ,
        MREQ_MEETING_RESPONSE,
        vec![WbxmlElement::text(PAGE_MREQ, MREQ_STATUS, "4")],
    );
    assert_eq!(
        parse_meeting_response_response(&top_level).expect("parse"),
        4
    );

    let empty = WbxmlElement::container(PAGE_MREQ, MREQ_MEETING_RESPONSE, vec![]);
    assert_eq!(parse_meeting_response_response(&empty).expect("parse"), 1);
}

/// A non-MeetingResponse root is a hard parse error (UnexpectedTag).
#[test]
fn meeting_response_response_rejects_wrong_root() {
    let wrong = WbxmlElement::container(PAGE_AIRSYNC, AS_SYNC, vec![]);
    assert!(parse_meeting_response_response(&wrong).is_err());
}

/// MeetingResponse status table per [MS-ASCMD] 2.2.3.177.9.
#[test]
fn meeting_response_status_message_maps_spec_table() {
    assert_eq!(meeting_response_status_message(1), "success");
    assert_eq!(
        meeting_response_status_message(2),
        "invalid meeting request"
    );
    assert_eq!(meeting_response_status_message(3), "server mailbox error");
    assert_eq!(meeting_response_status_message(4), "server error");
    // Out-of-table codes fall back to the common table.
    assert_eq!(meeting_response_status_message(150), "item not found");
    assert_eq!(meeting_response_status_message(999), "unknown status code");
}
