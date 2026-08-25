// SPDX-License-Identifier: MPL-2.0
use super::{WbxmlElement, WbxmlError, common_status_message, expect_tag, text_value};

// ============================================================================
// MeetingResponse (code page 8)
// ============================================================================
//
// Token table verified against [MS-ASWBXML] §2.1.2.1.9 (page 8):
//   CalendarId=0x05, CollectionId=0x06, MeetingResponse=0x07, RequestId=0x08,
//   Request=0x09, Result=0x0A, Status=0x0B, UserResponse=0x0C,
//   InstanceId=0x0E (14.1+), ProposedStartTime=0x10 / ProposedEndTime=0x11
//   (16.1), SendResponse=0x12 (16.0/16.1 only).

/// `MeetingResponse` code-page index (8).
pub const PAGE_MREQ: u8 = 8;
/// `CalendarId` (`MeetingResponse` page-8 token 0x05).
pub const MREQ_CALENDAR_ID: u8 = 0x05;
/// `CollectionId` (`MeetingResponse` page-8 token 0x06).
pub const MREQ_COLLECTION_ID: u8 = 0x06;
/// `MeetingResponse` (`MeetingResponse` page-8 token 0x07).
pub const MREQ_MEETING_RESPONSE: u8 = 0x07;
/// `RequestId` (`MeetingResponse` page-8 token 0x08).
pub const MREQ_REQUEST_ID: u8 = 0x08;
/// `Request` (`MeetingResponse` page-8 token 0x09).
pub const MREQ_REQUEST: u8 = 0x09;
/// `Result` (`MeetingResponse` page-8 token 0x0a).
pub const MREQ_RESULT: u8 = 0x0A;
/// `Status` (`MeetingResponse` page-8 token 0x0b).
pub const MREQ_STATUS: u8 = 0x0B;
/// `UserResponse` (`MeetingResponse` page-8 token 0x0c).
pub const MREQ_USER_RESPONSE: u8 = 0x0C;
/// `InstanceId` (`MeetingResponse` page-8 token 0x0e).
pub const MREQ_INSTANCE_ID: u8 = 0x0E; // 14.1+ per §2.1.2.1.9
/// `SendResponse` (`MeetingResponse` page-8 token 0x12).
pub const MREQ_SEND_RESPONSE: u8 = 0x12; // 16.0/16.1 only per §2.1.2.1.9

/// Build a MeetingResponse request ([MS-ASCMD] §2.2.1.11).
///
/// WBXML shape:
/// ```xml
/// <MeetingResponse>                    <!-- page 8, 0x07 -->
///   <Request>                          <!-- page 8, 0x09 -->
///     <UserResponse>{1|2|3}</>         <!-- 0x0C: 1=accept 2=tentative 3=decline -->
///     <CollectionId>{collection_id}</> <!-- 0x06: folder holding the invite email -->
///     <RequestId>{request_id}</>       <!-- 0x08: the invite EMAIL's ServerId -->
///     <InstanceId>{timestamp}</>       <!-- 0x0E, optional; only when instance_id -->
///     <SendResponse/>                  <!-- 0x12, empty; only when send_response -->
///   </Request>
/// </MeetingResponse>
/// ```
///
/// Child order: the §6.25 schema declares Request's children as `xs:all`
/// (order-insensitive), but we serialize in the documented element-table
/// order (UserResponse, CollectionId, RequestId, InstanceId, SendResponse) —
/// matching the Android6-Gmail `EasSendMeetingResponse.makeResponse`
/// reference (`MREQ_USER_RESPONSE`, `MREQ_COLLECTION_ID`, `MREQ_REQ_ID`;
/// Android is 14.1-era so it has no SendResponse slot). `RequestId` is the
/// meeting request EMAIL message's ServerId, not a calendar item id
/// (§2.2.1.11).
///
/// `InstanceId` (§2.2.3.92.1, 14.1+ per [MS-ASWBXML] §2.1.2.1.9) names ONE
/// instance of a recurring meeting by its ORIGINAL, unmodified UTC start time
/// as a 24-char [MS-ASCAL]-format timestamp. When `instance_id` is `None`
/// the response applies to EVERY instance of the recurring item (§2.2.3.92.1)
/// — single-instance meeting requests (the only kind the mail banner answers
/// today) always pass `None`.
///
/// `SendResponse` (16.0/16.1 only per [MS-ASWBXML] §2.1.2.1.9) is an empty
/// element whose presence asks the server to email the organizer; callers on
/// older protocol versions must pass `send_response: false` (the IPC layer
/// gates on the negotiated version).
pub fn build_meeting_response_request(
    collection_id: &str,
    request_id: &str,
    user_response: &str,
    instance_id: Option<&str>,
    send_response: bool,
) -> WbxmlElement {
    let mut request_children = vec![
        WbxmlElement::text(PAGE_MREQ, MREQ_USER_RESPONSE, user_response),
        WbxmlElement::text(PAGE_MREQ, MREQ_COLLECTION_ID, collection_id),
        WbxmlElement::text(PAGE_MREQ, MREQ_REQUEST_ID, request_id),
    ];
    if let Some(instance_id) = instance_id {
        request_children.push(WbxmlElement::text(PAGE_MREQ, MREQ_INSTANCE_ID, instance_id));
    }
    if send_response {
        request_children.push(WbxmlElement::empty(PAGE_MREQ, MREQ_SEND_RESPONSE));
    }
    WbxmlElement::container(
        PAGE_MREQ,
        MREQ_MEETING_RESPONSE,
        vec![WbxmlElement::container(
            PAGE_MREQ,
            MREQ_REQUEST,
            request_children,
        )],
    )
}

/// Parse a MeetingResponse response. Returns the per-Result Status code
/// ([MS-ASCMD] §6.26 response schema: `MeetingResponse > Result > Status`,
/// Status required 1...1 per §2.2.3.177.9). The first Result's Status wins;
/// a defensive top-level Status fallback covers servers that skip the Result
/// wrapper, and an entirely Status-less response defaults to 1 (success),
/// matching the convention of the other parsers in this file. Non-success
/// statuses are data, not parse failures — the client call site surfaces them
/// as `EasError::CommandStatus`.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_meeting_response_response(root: &WbxmlElement) -> Result<u32, WbxmlError> {
    expect_tag(root, PAGE_MREQ, MREQ_MEETING_RESPONSE)?;

    for child in &root.children {
        if (child.page, child.token) != (PAGE_MREQ, MREQ_RESULT) {
            continue;
        }
        for c in &child.children {
            if (c.page, c.token) == (PAGE_MREQ, MREQ_STATUS) {
                let s = text_value(c)?;
                return s
                    .parse::<u32>()
                    .map_err(|_| WbxmlError::InvalidContent(format!("non-numeric status: {s}")));
            }
        }
    }
    // Fallback: top-level Status (off-schema but harmless to accept).
    for child in &root.children {
        if (child.page, child.token) == (PAGE_MREQ, MREQ_STATUS) {
            let s = text_value(child)?;
            return s
                .parse::<u32>()
                .map_err(|_| WbxmlError::InvalidContent(format!("non-numeric status: {s}")));
        }
    }
    Ok(1) // success default
}

/// MeetingResponse status codes per [MS-ASCMD] 2.2.3.177.9 (1 = success,
/// 2 = invalid meeting request, 3 = server mailbox error, 4 = server error).
/// Out-of-table codes fall back to `common_status_message`.
pub fn meeting_response_status_message(status: u32) -> &'static str {
    match status {
        1 => "success",
        2 => "invalid meeting request",
        3 => "server mailbox error",
        4 => "server error",
        _ => common_status_message(status).unwrap_or("unknown status code"),
    }
}
