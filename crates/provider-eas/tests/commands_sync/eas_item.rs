// SPDX-License-Identifier: MPL-2.0
//! EasItem typed-struct serde shape, attachment fields, and Email/Email2 tag constants.

use super::*;

/// `EasItem` is now a typed struct (not a HashMap). Default has empty
/// server_id, None subject, no attachments, `has_attachments = false`.
#[test]
fn eas_item_is_typed_struct_with_expected_fields() {
    let item = EasItem::default();
    assert_eq!(item.server_id, "");
    assert_eq!(item.subject, None);
    assert_eq!(item.from, None);
    assert_eq!(item.to, None);
    assert_eq!(item.cc, None);
    assert_eq!(item.bcc, None);
    assert_eq!(item.reply_to, None);
    assert_eq!(item.date_received, None);
    assert_eq!(item.read, None);
    assert_eq!(item.flag, None);
    assert_eq!(item.importance, None);
    assert_eq!(item.body_html, None);
    assert_eq!(item.body_text, None);
    assert_eq!(item.body_mime, None);
    assert_eq!(item.body_truncated, None);
    assert_eq!(item.preview, None);
    assert!(!item.has_attachments);
    assert!(item.attachments.is_empty());
    assert_eq!(item.conversation_id, None);
    assert_eq!(item.is_draft, None);
    assert_eq!(item.message_id, None);
    // Task 4 meeting fields default to None (absent on the wire).
    assert_eq!(item.message_class, None);
    assert_eq!(item.meeting_message_type, None);
    assert_eq!(item.meeting, None);
}

/// A fully-populated `EasItem` round-trips through serde, proving the
/// `camelCase` rename matches what the frontend TS interface expects.
#[test]
fn eas_item_round_trips_through_serde() {
    let item = EasItem {
        server_id: "1:abc".to_string(),
        subject: Some("Hello".to_string()),
        from: Some("a@example.com".to_string()),
        to: Some("c@example.com".to_string()),
        cc: None,
        bcc: None,
        reply_to: None,
        date_received: Some("2026-06-29T00:00:00.000Z".to_string()),
        read: Some(true),
        flag: Some(false),
        importance: Some(1),
        body_html: Some("<p>hi</p>".to_string()),
        body_text: Some("hi".to_string()),
        body_mime: None,
        body_truncated: Some(false),
        preview: Some("hi".to_string()),
        has_attachments: true,
        attachments: vec![EasAttachment {
            file_reference: "ref-1".to_string(),
            display_name: "file.txt".to_string(),
            method: Some(1),
            estimated_data_size: Some(42),
            content_type: Some("text/plain".to_string()),
            content_location: None,
            is_inline: false,
            content_id: None,
        }],
        conversation_id: Some(vec![0xDE, 0xAD]),
        is_draft: Some(false),
        message_id: Some("<msg@host>".to_string()),
        message_class: Some("IPM.Note".to_string()),
        meeting_message_type: None,
        meeting: None,
    };
    let json = serde_json::to_string(&item).expect("serialize");
    // camelCase rename evidence:
    assert!(
        json.contains("\"dateReceived\""),
        "date_received must serialize as dateReceived"
    );
    assert!(
        json.contains("\"bodyHtml\""),
        "body_html must serialize as bodyHtml"
    );
    assert!(
        json.contains("\"hasAttachments\""),
        "has_attachments must serialize as hasAttachments"
    );
    assert!(
        json.contains("\"conversationId\""),
        "conversation_id must serialize as conversationId"
    );
    assert!(
        json.contains("\"isDraft\""),
        "is_draft must serialize as isDraft"
    );
    assert!(
        json.contains("\"messageId\""),
        "message_id must serialize as messageId"
    );
    assert!(
        json.contains("\"estimatedDataSize\""),
        "EasAttachment.estimated_data_size must serialize as estimatedDataSize"
    );
    let back: EasItem = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.server_id, item.server_id);
    assert_eq!(back.subject, item.subject);
    assert_eq!(back.has_attachments, item.has_attachments);
    assert_eq!(back.attachments.len(), 1);
    assert_eq!(
        back.attachments[0].content_type.as_deref(),
        Some("text/plain")
    );
    assert_eq!(back.conversation_id, Some(vec![0xDE, 0xAD]));
}

/// Task 4: a legacy persisted/IPC shape WITHOUT the meeting fields must
/// still deserialize — the new fields are `#[serde(default)]` Options, so
/// pre-existing EasItem JSON keeps round-tripping after the upgrade.
#[test]
fn eas_item_legacy_shape_without_meeting_fields_deserializes() {
    let legacy =
        r#"{"serverId":"1:abc","subject":"old shape","hasAttachments":false,"attachments":[]}"#;
    let item: EasItem = serde_json::from_str(legacy).expect("legacy shape must deserialize");
    assert_eq!(item.server_id, "1:abc");
    assert_eq!(item.message_class, None);
    assert_eq!(item.meeting_message_type, None);
    assert_eq!(item.meeting, None);
}

/// Task 4: the meeting fields round-trip through serde in camelCase
/// (EasItem is also an IPC payload; the frontend reads camelCase keys).
#[test]
fn eas_item_meeting_fields_round_trip_through_serde() {
    let item = EasItem {
        server_id: "5:777".into(),
        message_class: Some("IPM.Schedule.Meeting.Request".into()),
        meeting_message_type: Some(1),
        meeting: Some(MeetingRequestInfo {
            start_time: Some("2026-08-06T09:00:00.000Z".into()),
            end_time: Some("2026-08-06T09:30:00.000Z".into()),
            location: Some("Boardroom 3".into()),
            organizer: Some("boss@example.com".into()),
            response_requested: Some(true),
            all_day_event: Some(false),
            instance_type: Some(0),
            uid: Some("040000008200E00074C5B7101A82E008".into()),
        }),
        ..Default::default()
    };
    let json = serde_json::to_string(&item).expect("serialize");
    assert!(
        json.contains("\"messageClass\""),
        "message_class must serialize camelCase"
    );
    assert!(json.contains("\"meetingMessageType\""));
    assert!(json.contains("\"meeting\""));
    assert!(json.contains("\"startTime\""));
    assert!(json.contains("\"responseRequested\""));
    let back: EasItem = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back.message_class.as_deref(),
        Some("IPM.Schedule.Meeting.Request")
    );
    assert_eq!(back.meeting_message_type, Some(1));
    assert_eq!(
        back.meeting, item.meeting,
        "MeetingRequestInfo must round-trip intact"
    );
}

/// `EasAttachment` gained `content_type`, `estimated_data_size` (now u32
/// per the typed contract), and `content_location`.
#[test]
fn eas_attachment_new_fields_default_none() {
    let a = EasAttachment::default();
    assert_eq!(a.content_type, None);
    assert_eq!(a.estimated_data_size, None);
    assert_eq!(a.content_location, None);
}

/// Email (page 2) tag constants exist at the documented hex values.
#[test]
fn email_tag_constants_match_spec() {
    use provider_eas::wbxml::tags::email;
    assert_eq!(email::PAGE, 2);
    assert_eq!(email::DATE_RECEIVED, 0x0F);
    assert_eq!(email::SUBJECT, 0x14);
    assert_eq!(email::READ, 0x15);
    assert_eq!(email::TO, 0x16);
    assert_eq!(email::CC, 0x17);
    assert_eq!(email::FROM, 0x18);
    assert_eq!(email::REPLY_TO, 0x19);
    assert_eq!(email::IMPORTANCE, 0x12);
    assert_eq!(email::FLAG, 0x3A);
    // Task 4 meeting-request tokens ([MS-ASWBXML] Email code page 2).
    assert_eq!(email::MESSAGE_CLASS, 0x13);
    assert_eq!(email::ALL_DAY_EVENT, 0x1A);
    assert_eq!(email::END_TIME, 0x1E);
    assert_eq!(email::INSTANCE_TYPE, 0x1F);
    assert_eq!(email::LOCATION, 0x21);
    assert_eq!(email::MEETING_REQUEST, 0x22);
    assert_eq!(email::ORGANIZER, 0x23);
    assert_eq!(email::RESPONSE_REQUESTED, 0x26);
    assert_eq!(email::START_TIME, 0x31);
}

/// Email2 (page 22) tag constants exist at the documented hex values.
#[test]
fn email2_tag_constants_match_spec() {
    use provider_eas::wbxml::tags::email2;
    assert_eq!(email2::PAGE, 22);
    assert_eq!(email2::CONVERSATION_ID, 0x09);
    assert_eq!(email2::IS_DRAFT, 0x15);
    assert_eq!(email2::BCC, 0x16);
    // Task 4: [MS-ASEMAIL] §2.2.2.47.
    assert_eq!(email2::MEETING_MESSAGE_TYPE, 0x13);
}
