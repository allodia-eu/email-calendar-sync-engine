// SPDX-License-Identifier: MPL-2.0
use provider_eas::{
    calendar_write::{
        CalendarEventWrite, build_calendar_application_data, build_fixed_offset_tzi_base64,
    },
    commands::{tests_common::*, *},
};

#[test]
fn sync_request_round_trips() {
    let req = SyncRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

#[test]
fn get_item_estimate_request_round_trips() {
    let req = GetItemEstimateRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-1".to_string(),
        class: "Email".to_string(),
        filter_age_days: 0,
    };
    let tree = build_get_item_estimate_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Spec-shape parse test per [MS-ASWBXML] §2.1.2.1.7 (code page 6) and
/// [MS-ASCMD] §6.21 (GetItemEstimate response schema):
///   GetItemEstimate(6,0x05) > Response(6,0x0D) > Collection(6,0x08)
///     > CollectionId(6,0x0A) + Estimate(6,0x0C)
/// Regression guard: the old parser used Response=0x06 (off-spec),
/// CollectionId=0x0C (that is Estimate) and Estimate=0x05 (that is the
/// root tag) — so every live response parsed to count 0 / empty id.
#[test]
fn get_item_estimate_response_parses() {
    const PAGE_GIE: u8 = 6;
    let response = WbxmlElement::container(
        PAGE_GIE,
        0x05, // GetItemEstimate root
        vec![WbxmlElement::container(
            PAGE_GIE,
            0x0D, // Response (spec token — NOT 0x06)
            vec![WbxmlElement::container(
                PAGE_GIE,
                0x08, // Collection
                vec![
                    WbxmlElement::text(PAGE_GIE, 0x0A, "col-1"), // CollectionId = 0x0A
                    WbxmlElement::text(PAGE_GIE, 0x0C, "42"),    // Estimate = 0x0C
                ],
            )],
        )],
    );
    let parsed = parse_get_item_estimate_response(&response).expect("parse");
    assert_eq!(parsed.count, 42);
    assert_eq!(parsed.collection_id, "col-1");
}

/// Final-review fix: the GIE response Status (page 6, token 0x0E — a
/// sibling of Collection inside Response per [MS-ASCMD] §6.21) must be
/// parsed into `GetItemEstimateResult.status`. Live evidence 2026-08-02:
/// Exchange 2019 answered Status 3 ("sync state not primed") for a fresh
/// collection; the old parser dropped it, so callers saw a count-0
/// "success" instead of the real status.
#[test]
fn get_item_estimate_response_parses_status() {
    const PAGE_GIE: u8 = 6;
    let response = WbxmlElement::container(
        PAGE_GIE,
        0x05, // GetItemEstimate root
        vec![WbxmlElement::container(
            PAGE_GIE,
            0x0D, // Response
            vec![
                WbxmlElement::text(PAGE_GIE, 0x0E, "3"), // Status, sibling of Collection
                WbxmlElement::container(
                    PAGE_GIE,
                    0x08, // Collection
                    vec![
                        WbxmlElement::text(PAGE_GIE, 0x0A, "5"), // CollectionId
                        WbxmlElement::text(PAGE_GIE, 0x0C, "0"), // Estimate
                    ],
                ),
            ],
        )],
    );
    let parsed = parse_get_item_estimate_response(&response).expect("parse");
    assert_eq!(parsed.status, 3);
    assert_eq!(parsed.count, 0);
    assert_eq!(parsed.collection_id, "5");
}

/// A response without a Status element defaults to 1 (success) so
/// pre-fix persisted shapes and minimal servers read as success.
#[test]
fn get_item_estimate_response_status_defaults_to_one_when_absent() {
    const PAGE_GIE: u8 = 6;
    let response = WbxmlElement::container(
        PAGE_GIE,
        0x05,
        vec![WbxmlElement::container(
            PAGE_GIE,
            0x0D,
            vec![WbxmlElement::container(
                PAGE_GIE,
                0x08,
                vec![
                    WbxmlElement::text(PAGE_GIE, 0x0A, "col-1"),
                    WbxmlElement::text(PAGE_GIE, 0x0C, "42"),
                ],
            )],
        )],
    );
    let parsed = parse_get_item_estimate_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.count, 42);
    assert_eq!(parsed.collection_id, "col-1");
}

/// Request-shape test per [MS-ASWBXML] §2.1.2.1.7 + [MS-ASCMD] §6.20
/// (request schema, 14.0+ form A): inside each Collection the elements are
///   airsync:SyncKey (page 0, 0x0B), CollectionId (page 6, 0x0A),
///   airsync:Options (page 0, 0x17) > airsync:FilterType (page 0, 0x18)
/// in that order. SyncKey/FilterType are AirSync-page tokens, NOT page 6;
/// CollectionId is 0x0A, NOT 0x0C (Estimate). There is no top-level Class
/// element in the 14.0+ form.
#[test]
fn get_item_estimate_request_uses_spec_pages_and_tokens() {
    const PAGE_GIE: u8 = 6;
    let req = GetItemEstimateRequest {
        collection_id: "col-9".to_string(),
        sync_key: "key-7".to_string(),
        class: "Email".to_string(),
        filter_age_days: 7,
    };
    let tree = build_get_item_estimate_request(&req);

    assert_eq!((tree.page, tree.token), (PAGE_GIE, 0x05)); // GetItemEstimate
    assert_eq!(tree.children.len(), 1);
    let collections = &tree.children[0];
    assert_eq!((collections.page, collections.token), (PAGE_GIE, 0x07)); // Collections
    assert_eq!(collections.children.len(), 1);
    let collection = &collections.children[0];
    assert_eq!((collection.page, collection.token), (PAGE_GIE, 0x08)); // Collection

    // Child 0: airsync:SyncKey on PAGE 0 (token 0x0B) — was wrongly page 6.
    let sync_key = &collection.children[0];
    assert_eq!(
        (sync_key.page, sync_key.token),
        (PAGE_AIRSYNC, AS_SYNC_KEY),
        "SyncKey must be the AirSync-page (0) token, not a page-6 token"
    );
    assert_eq!(text_value(sync_key).unwrap(), "key-7");

    // Child 1: CollectionId on page 6 token 0x0A — was wrongly 0x0C (Estimate).
    let collection_id = &collection.children[1];
    assert_eq!((collection_id.page, collection_id.token), (PAGE_GIE, 0x0A));
    assert_eq!(text_value(collection_id).unwrap(), "col-9");

    // Child 2: airsync:Options > airsync:FilterType (both page 0).
    let options = &collection.children[2];
    assert_eq!((options.page, options.token), (PAGE_AIRSYNC, AS_OPTIONS));
    let filter = &options.children[0];
    assert_eq!((filter.page, filter.token), (PAGE_AIRSYNC, 0x18)); // FilterType
    assert_eq!(text_value(filter).unwrap(), "7");

    // No top-level Class element in the 14.0+ request form (MS-ASWBXML
    // §2.1.2.1.7 note 1 + MS-ASCMD §6.20 form A).
    assert_eq!(
        collection.children.len(),
        3,
        "Collection must contain only SyncKey, CollectionId, Options"
    );
}

/// Golden-bytes test: the serialized GIE request must match this exact
/// wire vector, which Exchange 2019 accepted live (2026-08-02 — answered
/// with a well-formed GetItemEstimate response carrying Status 3 "sync
/// state not primed", proving the bytes decode + schema-validate).
/// Layout: page switches (0x00) into page 6 for the GIE elements, into
/// page 0 for airsync:SyncKey, back to 6 for CollectionId; every element
/// closed with END (0x01) after its STR_I content.
#[test]
fn get_item_estimate_request_matches_accepted_wire_bytes() {
    let req = GetItemEstimateRequest {
        collection_id: "13".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        filter_age_days: 0,
    };
    let tree = build_get_item_estimate_request(&req);
    let bytes = provider_eas::wbxml::serialize_tree(&tree).expect("serialize");
    let expected: &[u8] = &[
        0x03, 0x01, 0x6A, 0x00, // WBXML header
        0x00, 0x06, // SWITCH_PAGE 6 (GetItemEstimate)
        0x45, // GetItemEstimate (0x05|0x40)
        0x47, // Collections    (0x07|0x40)
        0x48, // Collection     (0x08|0x40)
        0x00, 0x00, // SWITCH_PAGE 0 (AirSync)
        0x4B, 0x03, 0x30, 0x00, 0x01, // SyncKey STR_I "0" + END
        0x00, 0x06, // SWITCH_PAGE 6
        0x4A, 0x03, 0x31, 0x33, 0x00, 0x01, // CollectionId STR_I "13" + END
        0x01, 0x01, 0x01, // END Collection, Collections, GetItemEstimate
    ];
    assert_eq!(
        bytes, expected,
        "GIE request bytes drifted from the Exchange-accepted vector"
    );
}

/// FilterType 0 ("all items") is the default — the builder must omit the
/// whole Options element rather than emit a redundant filter.
#[test]
fn get_item_estimate_request_omits_options_when_filter_is_zero() {
    let req = GetItemEstimateRequest {
        collection_id: "col-1".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        filter_age_days: 0,
    };
    let tree = build_get_item_estimate_request(&req);
    let collection = &tree.children[0].children[0];
    assert_eq!(
        collection.children.len(),
        2,
        "filter_age_days 0 must not emit an Options element"
    );
    assert!(
        collection
            .children
            .iter()
            .all(|c| !(c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)),
        "no airsync:Options expected"
    );
}

#[test]
fn sync_result_default_status_is_success() {
    let r = SyncResult::default();
    assert_eq!(r.status, 1, "default SyncResult.status must be 1 (success)");
    assert!(!r.more_available);
    assert!(r.added.is_empty());
    assert!(r.updated.is_empty());
    assert!(r.deleted_server_ids.is_empty());
}

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
        from: Some("a@b.com".to_string()),
        to: Some("c@d.com".to_string()),
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
            organizer: Some("boss@x.com".into()),
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

// ---- Phase 3a Task 2: parse_application_data ----

/// Fixture: a synthetic EAS email ApplicationData element carrying the
/// fields the MVP parser must surface. Built with the real `WbxmlElement`
/// constructors on the documented code pages so `tag_name()` dispatch in
/// `parse_application_data` resolves identically to a server-generated tree.
///
/// Tree shape (codes in `(page, token)` form):
/// ```text
/// ApplicationData (0, 0x1D)
///   ├── Subject     (2, 0x14) = "Hello World"
///   ├── From        (2, 0x18) = "alice@example.com"
///   ├── To          (2, 0x16) = "bob@example.com"
///   ├── Read        (2, 0x15) = "1"
///   └── Body        (17, 0x0A)
///       ├── Type    (17, 0x06) = "2"   (HTML)
///       └── Data    (17, 0x0B) = "<p>Hi</p>"
/// ```
/// The fixture intentionally omits the optional fields (Cc/Bcc/Flag/Attachments/
/// ConversationId/IsDraft) — those are exercised by the focused tests below.
#[test]
fn parse_application_data_populates_core_email_fields() {
    use provider_eas::wbxml::tags::{base, email, pages};

    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(email::PAGE, email::SUBJECT, "Hello World"),
            WbxmlElement::text(email::PAGE, email::FROM, "alice@example.com"),
            WbxmlElement::text(email::PAGE, email::TO, "bob@example.com"),
            WbxmlElement::text(email::PAGE, email::READ, "1"),
            WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "2"),
                    WbxmlElement::text(pages::BASE, base::DATA, "<p>Hi</p>"),
                ],
            ),
        ],
    );

    // Drive it through the public Sync-response parser entry point so the
    // server_id → application_data wiring (parse_item) is also covered.
    let item = parse_application_data_for_test("1:abc", &app_data);

    assert_eq!(item.server_id, "1:abc");
    assert_eq!(item.subject.as_deref(), Some("Hello World"));
    assert_eq!(item.from.as_deref(), Some("alice@example.com"));
    assert_eq!(item.to.as_deref(), Some("bob@example.com"));
    assert_eq!(item.read, Some(true));
    // Body Type 2 → HTML body slot populated, plain-text slot stays None.
    assert_eq!(item.body_html.as_deref(), Some("<p>Hi</p>"));
    assert_eq!(item.body_text, None);
    // No attachments in this fixture.
    assert!(!item.has_attachments);
    assert!(item.attachments.is_empty());
}

/// Convenience wrapper around the (currently stubbed) parser so the test
/// references the real function name. This mirrors the brief's
/// `parse_application_data(server_id, &elem) -> EasItem` signature.
fn parse_application_data_for_test(server_id: &str, elem: &WbxmlElement) -> EasItem {
    let mut item = EasItem {
        server_id: server_id.to_string(),
        ..Default::default()
    };
    parse_application_data(elem, &mut item);
    item
}

/// Body Type 1 (PlainText) must populate `body_text`, leaving `body_html` None.
#[test]
fn parse_application_data_body_type_1_is_plain_text() {
    use provider_eas::wbxml::tags::{base, pages};
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            pages::BASE,
            base::BODY,
            vec![
                WbxmlElement::text(pages::BASE, base::TYPE, "1"),
                WbxmlElement::text(pages::BASE, base::DATA, "plain body"),
                WbxmlElement::text(pages::BASE, base::TRUNCATED, "1"),
                WbxmlElement::text(pages::BASE, base::PREVIEW, "preview…"),
            ],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.body_text.as_deref(), Some("plain body"));
    assert_eq!(item.body_html, None);
    assert_eq!(item.body_truncated, Some(true));
    assert_eq!(item.preview.as_deref(), Some("preview…"));
}

/// A missing/unknown Body Type falls back to populating both body slots.
#[test]
fn parse_application_data_body_unknown_type_fills_both_slots() {
    use provider_eas::wbxml::tags::{base, pages};
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            pages::BASE,
            base::BODY,
            vec![WbxmlElement::text(pages::BASE, base::DATA, "mystery")],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.body_html.as_deref(), Some("mystery"));
    assert_eq!(item.body_text.as_deref(), Some("mystery"));
}

// ---- Task 3 (eas-p2-polish): Body Type 4 = raw MIME BLOB ----
//
// [MS-ASCMD] §2.2.3.110.3: when the Sync Options carry MIMESupport and a
// BodyPreference Type 4, the server returns the body as a MIME BLOB —
// airsyncbase:Body with Type=4 and the raw RFC 5322 message in Data
// (children Type/EstimatedDataSize/Truncated/Data per the same section).
// MIME is its own slot (`body_mime`); it must NOT also fill
// body_html/body_text. The unknown-type fallback-to-both behavior is
// reserved for types other than 1/2/4.

/// Body Type 4 (MIME BLOB) must populate `body_mime` ONLY, leaving
/// `body_html` and `body_text` None.
#[test]
fn parse_application_data_body_type_4_is_raw_mime_only() {
    use provider_eas::wbxml::tags::{base, pages};
    let raw_mime = "Received: from contoso.com\r\nFrom: Chris Gray <chris@contoso.com>\r\nSubject: opaque s + e\r\n\r\nbody";
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            pages::BASE,
            base::BODY,
            vec![
                WbxmlElement::text(pages::BASE, base::TYPE, "4"),
                WbxmlElement::text(pages::BASE, base::ESTIMATED_DATA_SIZE, "13813"),
                WbxmlElement::text(pages::BASE, base::TRUNCATED, "0"),
                WbxmlElement::text(pages::BASE, base::DATA, raw_mime),
            ],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(
        item.body_mime.as_deref(),
        Some(raw_mime),
        "Type 4 data must land in body_mime"
    );
    assert_eq!(
        item.body_html, None,
        "Type 4 must NOT also fill body_html — MIME is its own slot"
    );
    assert_eq!(item.body_text, None, "Type 4 must NOT also fill body_text");
    assert_eq!(item.body_truncated, None, "Truncated=0 → None");
}

/// An unknown Body Type (e.g. 7) still falls back to BOTH html and text
/// slots — the pre-Task-3 behavior, preserved for anything other than
/// 1/2/4 — and leaves `body_mime` None.
#[test]
fn parse_application_data_body_unknown_type_7_still_fills_both_slots() {
    use provider_eas::wbxml::tags::{base, pages};
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            pages::BASE,
            base::BODY,
            vec![
                WbxmlElement::text(pages::BASE, base::TYPE, "7"),
                WbxmlElement::text(pages::BASE, base::DATA, "mystery"),
            ],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.body_html.as_deref(), Some("mystery"));
    assert_eq!(item.body_text.as_deref(), Some("mystery"));
    assert_eq!(item.body_mime, None);
}

/// Flag with Status="2" → `flag = Some(true)` (active follow-up).
#[test]
fn parse_application_data_flag_active_when_status_is_2() {
    use provider_eas::wbxml::tags::email;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            email::PAGE,
            email::FLAG,
            vec![WbxmlElement::text(email::PAGE, 0x3B, "2")], // Flag:Status
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.flag, Some(true));
}

/// Flag present but Status != "2" → `flag = Some(false)` (cleared).
#[test]
fn parse_application_data_flag_inactive_when_status_not_2() {
    use provider_eas::wbxml::tags::email;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            email::PAGE,
            email::FLAG,
            vec![WbxmlElement::text(email::PAGE, 0x3B, "0")], // cleared
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.flag, Some(false));
}

/// No Flag element → `flag = None`.
#[test]
fn parse_application_data_flag_absent_is_none() {
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::text(2, 0x14, "Subject only")],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.flag, None);
}

/// Attachments container with one Attachment populates `attachments`,
/// sets `has_attachments = true`, and maps each AirSyncBase field.
#[test]
fn parse_application_data_attachments_populated() {
    use provider_eas::wbxml::tags::{base, pages};
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            pages::BASE,
            base::ATTACHMENTS,
            vec![WbxmlElement::container(
                pages::BASE,
                base::ATTACHMENT,
                vec![
                    WbxmlElement::text(pages::BASE, base::DISPLAY_NAME, "report.pdf"),
                    WbxmlElement::text(pages::BASE, base::FILE_REFERENCE, "ref-42"),
                    WbxmlElement::text(pages::BASE, base::METHOD, "1"),
                    WbxmlElement::text(pages::BASE, base::CONTENT_ID, "<cid-1>"),
                    WbxmlElement::text(pages::BASE, base::IS_INLINE, "0"),
                    WbxmlElement::text(pages::BASE, base::CONTENT_TYPE, "application/pdf"),
                    WbxmlElement::text(pages::BASE, base::ESTIMATED_DATA_SIZE, "4096"),
                    WbxmlElement::text(pages::BASE, base::CONTENT_LOCATION, "https://x/a.pdf"),
                ],
            )],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert!(item.has_attachments);
    assert_eq!(item.attachments.len(), 1);
    let a = &item.attachments[0];
    assert_eq!(a.display_name, "report.pdf");
    assert_eq!(a.file_reference, "ref-42");
    assert_eq!(a.method, Some(1));
    assert_eq!(a.content_id.as_deref(), Some("<cid-1>"));
    assert!(!a.is_inline);
    assert_eq!(a.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(a.estimated_data_size, Some(4096));
    assert_eq!(a.content_location.as_deref(), Some("https://x/a.pdf"));
}

/// Empty Attachments container → `has_attachments = false`, empty vec.
#[test]
fn parse_application_data_empty_attachments_has_none() {
    use provider_eas::wbxml::tags::{base, pages};
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::container(
            pages::BASE,
            base::ATTACHMENTS,
            vec![],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert!(!item.has_attachments);
    assert!(item.attachments.is_empty());
}

/// ConversationId (opaque) round-trips into `conversation_id: Vec<u8>`.
#[test]
fn parse_application_data_conversation_id_opaque() {
    use provider_eas::wbxml::tags::email2;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::opaque(
            email2::PAGE,
            email2::CONVERSATION_ID,
            vec![0xDE, 0xAD, 0xBE, 0xEF],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.conversation_id, Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
}

/// ConversationId carried as base64 **text** (page 22, token 0x09) — the form
/// many Exchange deployments serialize it in — must parse to a non-empty
/// `Some(Vec<u8>)`. The bytes are kept verbatim (no base64 decode); downstream
/// treats `conversation_id` as opaque bytes regardless of wire form.
///
/// Regression for the asymmetry where the old `opaque_value_opt` only matched
/// `WbxmlValue::Opaque` and silently dropped the text form.
#[test]
fn parse_application_data_conversation_id_text_form_is_kept() {
    use provider_eas::wbxml::tags::email2;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::text(
            email2::PAGE,
            email2::CONVERSATION_ID,
            "Y29udm8=", // arbitrary base64-looking string; kept verbatim
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    let cid = item
        .conversation_id
        .clone()
        .expect("text-form ConversationId must not be dropped");
    assert!(!cid.is_empty(), "non-empty text must yield non-empty bytes");
    assert_eq!(cid, b"Y29udm8=".to_vec());
}

/// A missing or empty ConversationId must parse to `None`, NOT `Some(vec![])`.
/// `Some([])` serializes as `"conversationId":[]` (empty array) which is
/// semantically wrong — empty != absent — and would mislead the frontend's
/// threading logic. This locks the `None`-on-empty contract.
///
/// Regression for the old `unwrap_or_default()` which turned a missing/opaque
/// value into `Some(vec![])`.
#[test]
fn parse_application_data_conversation_id_missing_or_empty_is_none() {
    use provider_eas::wbxml::tags::email2;

    // Case 1: no ConversationId element at all.
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::text(2, 0x14, "Subject only")],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(
        item.conversation_id, None,
        "absent ConversationId must be None, not Some(vec![])"
    );

    // Case 2: ConversationId present but empty (Empty value).
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::empty(email2::PAGE, email2::CONVERSATION_ID)],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(
        item.conversation_id, None,
        "empty ConversationId must be None, not Some(vec![])"
    );

    // Case 3: ConversationId present as empty opaque blob.
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::opaque(
            email2::PAGE,
            email2::CONVERSATION_ID,
            vec![],
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(
        item.conversation_id, None,
        "empty-opaque ConversationId must be None, not Some(vec![])"
    );

    // Case 4: ConversationId present as empty text string.
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::text(
            email2::PAGE,
            email2::CONVERSATION_ID,
            "",
        )],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(
        item.conversation_id, None,
        "empty-text ConversationId must be None, not Some(vec![])"
    );
}

/// IsDraft="1" → `Some(true)`; IsDraft="0" → `Some(false)`.
#[test]
fn parse_application_data_is_draft_flag() {
    use provider_eas::wbxml::tags::email2;
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(email2::PAGE, email2::IS_DRAFT, "1"),
            WbxmlElement::text(email2::PAGE, email2::BCC, "secret@example.com"),
        ],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.is_draft, Some(true));
    assert_eq!(item.bcc.as_deref(), Some("secret@example.com"));
}

/// Unknown tags are ignored — the parser must not panic or mis-dispatch.
#[test]
fn parse_application_data_ignores_unknown_tags() {
    // Use an unregistered (page, token) so tag_name() returns "unknown".
    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(0xFE, 0x7F, "garbage"),
            WbxmlElement::text(2, 0x14, "Real Subject"),
        ],
    );
    let item = parse_application_data_for_test("s1", &app_data);
    assert_eq!(item.subject.as_deref(), Some("Real Subject"));
}

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
                    WbxmlElement::text(email::PAGE, email::ORGANIZER, "boss@x.com"),
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
    assert_eq!(meeting.organizer.as_deref(), Some("boss@x.com"));
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

// ---- Phase 3a Task 3: build_sync_request emits BodyPreference ----

/// `build_sync_request` must emit an `Options/BodyPreference/Type=2` element
/// inside each `Collection` so the server returns HTML bodies (per
/// [MS-ASAIRSMB] AirSyncBase:BodyPreference). This test serializes the
/// built tree to WBXML bytes and back, then walks the deserialized tree to
/// prove the element survives a real round-trip — a pure structural
/// equality check would miss serializer/deserializer bugs.
#[test]
fn build_sync_request_emits_body_preference_type_2() {
    use provider_eas::wbxml::tags::{airsync, base, pages};

    let req = SyncRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);

    // Root: Sync (page 0, 0x05)
    assert_eq!(back.page, PAGE_AIRSYNC);
    assert_eq!(back.token, AS_SYNC);

    // Walk Collections → Collection.
    let collections = back
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)
        .expect("missing Collections container");
    let collection = collections
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)
        .expect("missing Collection element");

    // Options must be present inside the collection.
    let options = collection
        .children
        .iter()
        .find(|c| c.page == pages::AIRSYNC && c.token == airsync::OPTIONS)
        .expect("missing Options element inside Collection");
    assert_eq!(options.tag_name(), "Options");

    // BodyPreference inside Options.
    let body_pref = options
        .children
        .iter()
        .find(|c| c.page == pages::BASE && c.token == base::BODY_PREFERENCE)
        .expect("missing BodyPreference element inside Options");
    assert_eq!(body_pref.tag_name(), "BodyPreference");

    // Type child must be present with value "2" (HTML).
    let type_el = body_pref
        .children
        .iter()
        .find(|c| c.page == pages::BASE && c.token == base::TYPE)
        .expect("missing Type element inside BodyPreference");
    assert_eq!(type_el.tag_name(), "Type");
    match &type_el.value {
        WbxmlValue::Text(t) => assert_eq!(t, "2"),
        other => panic!(
            "expected Text value for BodyPreference/Type, got {:?}",
            other
        ),
    }
}

/// MS-ASAIRS 2.2.2.35.4: the server only returns `Body > Preview` (the
/// message-list snippet) when the request's BodyPreference carries a
/// Preview child (0-255 = max preview chars). Without it every EAS
/// message synced with an empty snippet (live finding 2026-08-04: 82/82
/// messages had no preview). The request must emit Preview=255 as the
/// LAST BodyPreference child (schema order: Type, TruncationSize,
/// AllOrNone, Preview).
#[test]
fn build_sync_request_requests_body_preview() {
    use provider_eas::wbxml::tags::{airsync, base, pages};

    let req = SyncRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: Some(200 * 1024),
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let collections = &tree.children[0];
    let collection = &collections.children[0];
    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == airsync::OPTIONS)
        .expect("missing Options");
    let body_pref = options
        .children
        .iter()
        .find(|c| c.page == pages::BASE && c.token == base::BODY_PREFERENCE)
        .expect("missing BodyPreference");
    let last = body_pref
        .children
        .last()
        .expect("no BodyPreference children");
    assert_eq!(
        (last.page, last.token),
        (pages::BASE, base::PREVIEW),
        "Preview must be the LAST BodyPreference child (schema order)"
    );
    match &last.value {
        WbxmlValue::Text(t) => assert_eq!(t, "255"),
        other => panic!("expected Text Preview, got {other:?}"),
    }
}

/// When `fetch_body` is false, the `Options/BodyPreference` block must be
/// omitted so the server doesn't waste bandwidth returning bodies.
#[test]
fn build_sync_request_omits_body_preference_when_fetch_body_false() {
    use provider_eas::wbxml::tags::{airsync, base, pages};

    let req = SyncRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);

    let collections = back
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)
        .expect("missing Collections container");
    let collection = collections
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)
        .expect("missing Collection element");

    let has_body_pref = collection.children.iter().any(|c| {
        c.page == pages::BASE && c.token == base::BODY_PREFERENCE
            || (c.page == pages::AIRSYNC
                && c.token == airsync::OPTIONS
                && c.children
                    .iter()
                    .any(|o| o.page == pages::BASE && o.token == base::BODY_PREFERENCE))
    });
    assert!(
        !has_body_pref,
        "BodyPreference should NOT be emitted when fetch_body=false"
    );
}

// ---- Phase A Task 7: BodyPreference TruncationSize ----

/// Walk the round-tripped tree to the `Options/BodyPreference` element,
/// or None when the block is absent. Shared by the TruncationSize tests.
fn find_body_preference(back: &WbxmlElement) -> Option<&WbxmlElement> {
    use provider_eas::wbxml::tags::{airsync, base, pages};

    let collections = back
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)?;
    let collection = collections
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)?;
    let options = collection
        .children
        .iter()
        .find(|c| c.page == pages::AIRSYNC && c.token == airsync::OPTIONS)?;
    options
        .children
        .iter()
        .find(|c| c.page == pages::BASE && c.token == base::BODY_PREFERENCE)
}

/// When `truncation_size` is Some AND `fetch_body` is true, the
/// BodyPreference container must carry an AirSyncBase `TruncationSize`
/// child holding the byte cap. Token 0x07 on code page 17 — verified
/// against docs/Exchange/MS-ASWBXML.txt §2.1.2.1.18 (AirSyncBase table:
/// BodyPreference 0x05, Type 0x06, TruncationSize 0x07). 204800 (200KB)
/// is Android's value.
#[test]
fn build_sync_request_emits_truncation_size_when_set() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: true,
        truncation_size: Some(204800),
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);

    let body_pref = find_body_preference(&back).expect("missing BodyPreference element");
    let trunc = body_pref
        .children
        .iter()
        .find(|c| c.page == pages::BASE && c.token == base::TRUNCATION_SIZE)
        .expect("missing TruncationSize element inside BodyPreference");
    assert_eq!(trunc.tag_name(), "TruncationSize");
    match &trunc.value {
        WbxmlValue::Text(t) => assert_eq!(t, "204800"),
        other => panic!(
            "expected Text value for BodyPreference/TruncationSize, got {:?}",
            other
        ),
    }
}

/// `truncation_size: None` must omit the TruncationSize element — the
/// BodyPreference block stays byte-for-byte identical to the pre-Task-7
/// shape (Type only), so servers that predate the field see no change.
#[test]
fn build_sync_request_omits_truncation_size_when_none() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);

    let body_pref = find_body_preference(&back).expect("missing BodyPreference element");
    assert!(
        body_pref
            .children
            .iter()
            .all(|c| !(c.page == pages::BASE && c.token == base::TRUNCATION_SIZE)),
        "TruncationSize should NOT be emitted when truncation_size is None"
    );
}

/// `truncation_size` is gated on `fetch_body`: with `fetch_body: false`
/// the whole BodyPreference block is omitted, so TruncationSize must not
/// appear anywhere in the request either.
#[test]
fn build_sync_request_omits_truncation_size_when_fetch_body_false() {
    let req = SyncRequest {
        collection_id: "col-1".to_string(),
        sync_key: "key-0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: false,
        truncation_size: Some(204800),
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);

    assert!(
        find_body_preference(&back).is_none(),
        "BodyPreference (and thus TruncationSize) should NOT be emitted when fetch_body=false"
    );
}

// ---- Phase 3a Task 5: top-level parse_sync_response orchestration ----
//
// Tasks 1-2 covered `parse_application_data` (ApplicationData -> EasItem).
// Task 3 covered request building. Task 4 covered `eas_item_to_remote` and
// `sync_folder`'s status-3 recovery branch. This block locks the
// top-level orchestration that those tasks did NOT exercise:
//   * Sync -> Collections -> Collection traversal
//   * SyncKey / Status / MoreAvailable extraction at the Collection level
//   * Commands -> Add/Change/Delete dispatch into `added` / `updated` / `deleted_server_ids`
//
// The fixture trees are built with the real `WbxmlElement` constructors on
// the documented code pages (AirSync=0, Email=2, AirSyncBase=17), so
// `tag_name()` dispatch resolves identically to a server-generated tree.

/// Build a single EAS email `ApplicationData` element carrying Subject +
/// From + To + Body[Type=2 HTML]. Shared by the Add and Change fixtures
/// below so the test body stays focused on the top-level orchestration.
fn fixture_email_app_data(subject: &str, from: &str, to: &str, body_html: &str) -> WbxmlElement {
    use provider_eas::wbxml::tags::{base, email, pages};
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(email::PAGE, email::SUBJECT, subject),
            WbxmlElement::text(email::PAGE, email::FROM, from),
            WbxmlElement::text(email::PAGE, email::TO, to),
            WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "2"),
                    WbxmlElement::text(pages::BASE, base::DATA, body_html),
                ],
            ),
        ],
    )
}

/// Full Sync-response fixture: Sync -> Collections -> Collection with
/// SyncKey="{sk1}", Status="1", MoreAvailable, and a Commands block
/// containing one Add (ServerId "1:1" + the email ApplicationData above).
///
/// Asserts the entire top-level orchestration path: sync_key, status,
/// more_available, and the added/updated/deleted vectors are populated by
/// walking the real tree through `parse_sync_response`.
#[test]
fn parse_sync_response_extracts_full_sync_collection() {
    let add_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "1:1"),
            fixture_email_app_data("Hello", "a@b", "c@d", "<p>hi</p>"),
        ],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![add_cmd]);
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{sk1}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE),
            commands,
        ],
    );
    let collections = WbxmlElement::container(PAGE_AIRSYNC, AS_COLLECTIONS, vec![collection]);
    let tree = WbxmlElement::container(PAGE_AIRSYNC, AS_SYNC, vec![collections]);

    let result = parse_sync_response(&tree).expect("parse_sync_response must succeed");

    // Top-level orchestration fields.
    assert_eq!(result.sync_key, "{sk1}");
    assert_eq!(
        result.status, 1,
        "success status must surface from Collection/Status"
    );
    assert!(
        result.more_available,
        "MoreAvailable element must set more_available=true"
    );

    // Added item: full envelope must round-trip through parse_item ->
    // parse_application_data (covered in depth by Task 2; here we lock the
    // Add-dispatch wiring at the Commands level).
    assert_eq!(result.added.len(), 1, "exactly one Add command");
    let added = &result.added[0];
    assert_eq!(added.server_id, "1:1");
    assert_eq!(added.subject.as_deref(), Some("Hello"));
    assert_eq!(added.from.as_deref(), Some("a@b"));
    assert_eq!(added.to.as_deref(), Some("c@d"));
    assert_eq!(
        added.body_html.as_deref(),
        Some("<p>hi</p>"),
        "Body Type=2 must populate body_html"
    );

    // No Change/Delete in this fixture.
    assert!(result.updated.is_empty(), "no Change commands in fixture");
    assert!(
        result.deleted_server_ids.is_empty(),
        "no Delete commands in fixture"
    );
}

/// A Commands block with Change + Delete must populate `updated` and
/// `deleted_server_ids` respectively, and leave `added` empty.
#[test]
fn parse_sync_response_dispatches_change_and_delete() {
    let change_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_CHANGE,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "2:2"),
            fixture_email_app_data("Updated", "x@y", "z@w", "<p>u</p>"),
        ],
    );
    // EAS Delete is a CONTAINER carrying the ServerId as a child element
    // (MS-ASCMD 2.2.3.42.2), the same shape Add/Change use.
    let delete_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_DELETE,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "3:3")],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![change_cmd, delete_cmd]);
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{sk2}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            commands,
        ],
    );
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert!(result.added.is_empty(), "no Add in this fixture");
    assert_eq!(result.updated.len(), 1, "one Change");
    assert_eq!(result.updated[0].server_id, "2:2");
    assert_eq!(
        result.deleted_server_ids,
        vec!["3:3".to_string()],
        "Delete ServerId must land in deleted_server_ids"
    );
    // No MoreAvailable in this fixture.
    assert!(
        !result.more_available,
        "MoreAvailable absent must remain false"
    );
}

/// Status-recovery parse lock: a Collection carrying `Status = "3"`
/// (invalid sync key, per MS-ASSYNC 2.2.3.23) must surface on
/// `SyncResult.status` so `EasSource::sync_folder`'s resync branch can act
/// on it. Task 4 covered the *behavioral* recovery; this test locks the
/// *parse-level* status plumbing that feeds it.
///
/// Without the parser surfacing Status, `result.status` would stay at the
/// `SyncResult::default()` value of `1` regardless of the wire value, and
/// the resync branch would never fire on a real status-3 response.
#[test]
fn parse_sync_response_surfaces_collection_status_3() {
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{stale}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "3"),
        ],
    );
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert_eq!(
        result.status, 3,
        "Collection/Status=3 must surface on SyncResult.status so sync_folder can resync"
    );
    assert_eq!(result.sync_key, "{stale}");
    // A status-3 response typically carries no Commands; assert the
    // vectors stay empty so the engine's resync path (which wipes the
    // cache and re-enters with sync_key "0") is not fed stale items.
    assert!(result.added.is_empty());
    assert!(result.updated.is_empty());
    assert!(result.deleted_server_ids.is_empty());
}

/// `parse_sync_response` must reject a tree whose root is not
/// Sync (page 0, token 0x05) with `WbxmlError::UnexpectedTag`. This locks
/// the `expect_tag` guard so a misrouted response (e.g. a FolderSync tree
/// handed to the Sync parser) fails loudly rather than returning a default
/// `SyncResult` that looks like success.
#[test]
fn parse_sync_response_rejects_non_sync_root() {
    let wrong_root = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    let err = parse_sync_response(&wrong_root).expect_err("must reject non-Sync root");
    assert!(
        matches!(err, WbxmlError::UnexpectedTag { .. }),
        "expected UnexpectedTag, got {err:?}"
    );
}

/// An empty Sync tree (root with no Collections child) must parse
/// successfully and yield a default `SyncResult` (status=1, empty vectors,
/// sync_key=""). This is the shape a server returns when it has nothing to
/// say; the engine must treat it as a no-op success, not an error.
#[test]
fn parse_sync_response_empty_tree_is_default_success() {
    let tree = WbxmlElement::container(PAGE_AIRSYNC, AS_SYNC, vec![]);
    let result = parse_sync_response(&tree).expect("parse");
    assert_eq!(result.status, 1, "default status is success");
    assert_eq!(result.sync_key, "");
    assert!(!result.more_available);
    assert!(result.added.is_empty());
    assert!(result.updated.is_empty());
    assert!(result.deleted_server_ids.is_empty());
}

// ---- R2 Task 4: live-probe findings (2026-08-02) ----
//
// The live probe against Exchange 2019 (16.1) showed `status 1, key
// <empty>, added 0` for the Sync bootstrap. Raw-dump evidence
// (examples/eas_sync_debug.rs): the server actually replied
// `Sync/Status=4` (protocol error) with NO Collections element —
// `x-ms-aserror: <Collection> node contains child node <Class> which
// appears out of order`. Two defects combined to hide that:
//   1. build_sync_request appended `<Class>` as the LAST child of `<Collection>` (after
//      `<Options>`). Per [MS-ASSYNC] the Class element is not a valid Collection child in protocol
//      14.0+ (CollectionId identifies the collection), so Exchange 16.1 rejects the whole request.
//   2. parse_sync_response ignored the top-level `<Status>`, so the rejection surfaced as a default
//      success with an empty sync key instead of an error the engine could act on.

/// `build_sync_request` must NOT emit an `airsync:Class` element inside
/// `Collection`. Live evidence (Exchange 2019, protocol 16.1): sending it
/// makes the server reject the request with top-level Status=4
/// ("<Class> ... appears out of order"). Per [MS-ASSYNC] §2.2.2.11 the
/// Class element is only valid in protocol 2.5/12.x; `CollectionId`
/// identifies the collection in 14.0+.
#[test]
fn build_sync_request_omits_class_element() {
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);

    let collections = back
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)
        .expect("missing Collections container");
    let collection = collections
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)
        .expect("missing Collection element");

    let has_class = collection
        .children
        .iter()
        .any(|c| c.page == PAGE_AIRSYNC && c.token == AS_CLASS);
    assert!(
        !has_class,
        "Class must NOT be emitted in a Sync Collection (16.1 rejects it)"
    );

    // CollectionId must still be present — it is what identifies the
    // collection now that Class is gone.
    let collection_id = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION_ID)
        .expect("missing CollectionId");
    assert_eq!(
        collection_id.value,
        WbxmlValue::Text("22".to_string()),
        "CollectionId identifies the collection in 14.0+"
    );
}

/// A Sync response whose root carries `<Status>4</Status>` and NO
/// `Collections` element (Exchange's request-rejection shape — live
/// evidence: eas_sync_debug raw dump) must surface that status on
/// `SyncResult.status`, not read as default success with an empty key.
/// Collection-level Status (when present) remains authoritative.
#[test]
fn parse_sync_response_surfaces_top_level_status() {
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "4")],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert_eq!(
        result.status, 4,
        "top-level Sync/Status=4 (request rejected) must surface, not read as success"
    );
    assert!(result.sync_key.is_empty());
    assert!(result.added.is_empty());
}

/// When BOTH a top-level and a collection-level Status are present, the
/// collection-level value wins — it is the more specific signal.
#[test]
fn parse_sync_response_collection_status_overrides_top_level() {
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "2"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "3"),
        ],
    );
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            WbxmlElement::container(PAGE_AIRSYNC, AS_COLLECTIONS, vec![collection]),
        ],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert_eq!(result.status, 3, "collection-level Status is authoritative");
    assert_eq!(result.sync_key, "2");
}

/// On protocol 16.1 `build_sync_request` must NOT emit a `GetChanges`
/// element. [MS-ASSYNC] §2.2.2.9: GetChanges is not valid in 16.1 — the
/// server sends changes by default. Live evidence (eas_sync_bisect,
/// Exchange 2019, 2026-08-02): EVERY request variant containing
/// GetChanges — bare token, empty container, with/without WindowSize or
/// DeletesAsMoves — was rejected with top-level Status=4; the identical
/// request minus GetChanges returned Status=1 with a real SyncKey.
#[test]
fn build_sync_request_omits_get_changes_on_16_1() {
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "16.1");
    let back = round_trip(&tree);

    let collections = back
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)
        .expect("missing Collections container");
    let collection = collections
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)
        .expect("missing Collection element");

    let has_get_changes = collection
        .children
        .iter()
        .any(|c| c.page == PAGE_AIRSYNC && c.token == AS_GET_CHANGES);
    assert!(
        !has_get_changes,
        "GetChanges must NOT be emitted on protocol 16.1 (server rejects it)"
    );
}

/// On pre-16.1 protocols GetChanges is REQUIRED to receive changes —
/// omitting it there would silently sync nothing. Lock the version gate.
#[test]
fn build_sync_request_emits_get_changes_on_14_0() {
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let tree = build_sync_request(&req, "14.0");
    let back = round_trip(&tree);

    let collections = back
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)
        .expect("missing Collections container");
    let collection = collections
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)
        .expect("missing Collection element");

    let has_get_changes = collection
        .children
        .iter()
        .any(|c| c.page == PAGE_AIRSYNC && c.token == AS_GET_CHANGES);
    assert!(
        has_get_changes,
        "GetChanges must be emitted on pre-16.1 protocols"
    );
}

// ---- Task 2 (eas-p2-polish): explicit Sync options — DeletesAsMoves,
// FilterType, WindowSize default 100 ----
//
// Spec anchors (docs/Exchange/mscmd.txt, [MS-ASCMD] v20250520):
// - §2.2.1.21 Collection strict child order: SyncKey, CollectionId, Supported, DeletesAsMoves,
//   GetChanges, WindowSize, ConversationMode, Options, Commands. This builder never emits
//   ConversationMode and emits Supported only when `SyncRequest::supported` is set; the tests below
//   send no Supported, so the emitted subsequence is SyncKey, CollectionId, DeletesAsMoves,
//   GetChanges?, WindowSize, Options?.
// - §4.5.1.1: every Sync example request sends `<DeletesAsMoves/>`.
// - §2.2.3.43: an empty or absent DeletesAsMoves means TRUE — deletes move to the Deleted Items
//   folder (the server default, made explicit on the wire so intent is never
//   server-version-dependent).
// - §2.2.3.125.6 Options (Sync): FilterType is the FIRST child, ahead of BodyPreference (task-brief
//   order: FilterType?, Class?, ConversationMode?, MaxItems?, BodyPreference*, MIMESupport?,
//   MIMETruncation?, RightsManagementSupport?).
// - §2.2.3.68.2 FilterType (Sync): 0 = no filter, so 0 omits the element.

/// Build a Sync request, round-trip it through the WBXML codec, and
/// return the `Collection` element for positional assertions.
fn sync_collection_for(req: &SyncRequest, protocol_version: &str) -> WbxmlElement {
    let tree = build_sync_request(req, protocol_version);
    let back = round_trip(&tree);
    let collections = back
        .children
        .into_iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)
        .expect("missing Collections container");
    collections
        .children
        .into_iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)
        .expect("missing Collection element")
}

/// `(page, token)` sequence of a Collection's children, for exact-order
/// assertions.
fn collection_child_tokens(collection: &WbxmlElement) -> Vec<(u8, u8)> {
    collection
        .children
        .iter()
        .map(|c| (c.page, c.token))
        .collect()
}

/// On 14.0 (GetChanges emitted) `DeletesAsMoves` must sit immediately
/// after CollectionId and immediately before GetChanges — the strict
/// Collection child order of [MS-ASCMD] §2.2.1.21.
#[test]
fn build_sync_request_emits_deletes_as_moves_between_collection_id_and_get_changes_on_14_0() {
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "14.0");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_GET_CHANGES),
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "DeletesAsMoves must follow CollectionId and precede GetChanges (§2.2.1.21 order)"
    );

    // Empty form `<DeletesAsMoves/>` — value TRUE per §2.2.3.43.
    let deletes_as_moves = &collection.children[2];
    assert_eq!(deletes_as_moves.value, WbxmlValue::Empty);
    assert_eq!(deletes_as_moves.tag_name(), "DeletesAsMoves");
}

/// On 16.1 (no GetChanges) `DeletesAsMoves` must still be emitted and
/// must sit immediately after CollectionId.
#[test]
fn build_sync_request_emits_deletes_as_moves_after_collection_id_on_16_1() {
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "DeletesAsMoves must follow CollectionId on 16.1 too (§2.2.1.21 order)"
    );

    let deletes_as_moves = &collection.children[2];
    assert_eq!(deletes_as_moves.value, WbxmlValue::Empty);
    assert_eq!(deletes_as_moves.tag_name(), "DeletesAsMoves");
}

/// When `filter_age_days != 0` and bodies are fetched, Options must carry
/// `FilterType` as its FIRST child with BodyPreference after it
/// (§2.2.3.125.6 / task-brief Options order).
#[test]
fn build_sync_request_emits_filter_type_first_in_options_with_body_preference() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 3,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options element inside Collection");
    assert_eq!(
        options.children.len(),
        2,
        "Options must hold exactly FilterType + BodyPreference, got {:?}",
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>()
    );

    let filter = &options.children[0];
    assert_eq!(
        (filter.page, filter.token),
        (PAGE_AIRSYNC, 0x18),
        "FilterType must be the FIRST Options child (page 0, 0x18)"
    );
    assert_eq!(filter.tag_name(), "FilterType");
    assert_eq!(filter.value, WbxmlValue::Text("3".to_string()));

    let body_pref = &options.children[1];
    assert_eq!(
        (body_pref.page, body_pref.token),
        (pages::BASE, base::BODY_PREFERENCE),
        "BodyPreference must follow FilterType inside Options"
    );
}

/// `fetch_body: false` with `filter_age_days != 0`: Options must STILL
/// be emitted — with ONLY FilterType — so the age filter applies to
/// header-only sync rounds too.
#[test]
fn build_sync_request_emits_options_with_only_filter_type_when_fetch_body_false() {
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("Options must be emitted when filter_age_days != 0 even with fetch_body=false");
    assert_eq!(
        options.children.len(),
        1,
        "Options must contain ONLY FilterType when fetch_body=false, got {:?}",
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>()
    );
    let filter = &options.children[0];
    assert_eq!((filter.page, filter.token), (PAGE_AIRSYNC, 0x18));
    assert_eq!(filter.value, WbxmlValue::Text("7".to_string()));
}

/// Default production shape (engine drain loop: `filter_age_days: 0`,
/// `fetch_body: true`): DeletesAsMoves is present (unconditional), but
/// FilterType stays omitted and Options keeps its single BodyPreference
/// child — no wire regression beyond the new DeletesAsMoves element.
#[test]
fn build_sync_request_default_shape_has_deletes_as_moves_and_no_filter_type() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 100,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let has_deletes_as_moves = collection
        .children
        .iter()
        .any(|c| c.page == PAGE_AIRSYNC && c.token == 0x1E);
    assert!(
        has_deletes_as_moves,
        "DeletesAsMoves is unconditional — spec examples always send it"
    );

    let has_filter_type = collection.children.iter().any(|c| {
        (c.page == PAGE_AIRSYNC && c.token == 0x18)
            || (c.page == PAGE_AIRSYNC
                && c.token == AS_OPTIONS
                && c.children
                    .iter()
                    .any(|o| o.page == PAGE_AIRSYNC && o.token == 0x18))
    });
    assert!(
        !has_filter_type,
        "FilterType must NOT be emitted when filter_age_days=0 (0 = no filter, §2.2.3.68.2)"
    );

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options");
    assert_eq!(
        options.children.len(),
        1,
        "Options keeps its single BodyPreference child for filter_age_days=0"
    );
    assert_eq!(
        (options.children[0].page, options.children[0].token),
        (pages::BASE, base::BODY_PREFERENCE)
    );
}

// ---- Task 3 (eas-p2-polish): MIMESupport + MIMETruncation in Sync Options ----
//
// Spec anchors (docs/Exchange/mscmd.txt, [MS-ASCMD] v20250520):
// - §2.2.3.125.6 Options (Sync) child set: FilterType, …, BodyPreference*, MIMESupport?,
//   MIMETruncation?, … — MIMESupport/MIMETruncation go AFTER BodyPreference (task-brief order:
//   FilterType?, Class?, ConversationMode?, MaxItems?, BodyPreference*, MIMESupport?,
//   MIMETruncation?, RightsManagementSupport?).
// - §2.2.3.110.3 MIMESupport (Sync): 0 = never send MIME, 1 = S/MIME messages only, 2 = all
//   messages; absent defaults to 0.
// - §2.2.3.111 MIMETruncation: levels 0-8 (0 = truncate all … 8 = send complete MIME data).
// - Both are AirSync-page (0) tokens: MIMESupport 0x22, MIMETruncation 0x23 (verified in
//   code_pages.rs AIRSYNC_TOKENS).

/// With a filter AND bodies, Options children must be exactly
/// [FilterType, BodyPreference, MIMESupport, MIMETruncation] in that
/// order — MIMESupport/MIMETruncation follow BodyPreference
/// (§2.2.3.125.6).
#[test]
fn build_sync_request_emits_mime_elements_after_body_preference_with_filter() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: true,
        truncation_size: None,
        mime_support: Some(1),
        mime_truncation: Some(4),
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options element inside Collection");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![
            (PAGE_AIRSYNC, 0x18), // FilterType
            (pages::BASE, base::BODY_PREFERENCE),
            (PAGE_AIRSYNC, 0x22), // MIMESupport
            (PAGE_AIRSYNC, 0x23), // MIMETruncation
        ],
        "Options order must be FilterType, BodyPreference, MIMESupport, MIMETruncation (§2.2.3.125.6)"
    );

    let mime_support = &options.children[2];
    assert_eq!(mime_support.tag_name(), "MIMESupport");
    assert_eq!(mime_support.value, WbxmlValue::Text("1".to_string()));

    let mime_truncation = &options.children[3];
    assert_eq!(mime_truncation.tag_name(), "MIMETruncation");
    assert_eq!(mime_truncation.value, WbxmlValue::Text("4".to_string()));
}

/// Without a filter (the production default), Options children must be
/// exactly [BodyPreference, MIMESupport, MIMETruncation].
#[test]
fn build_sync_request_emits_mime_elements_after_body_preference_without_filter() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: Some(1),
        mime_truncation: Some(4),
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options element inside Collection");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![
            (pages::BASE, base::BODY_PREFERENCE),
            (PAGE_AIRSYNC, 0x22), // MIMESupport
            (PAGE_AIRSYNC, 0x23), // MIMETruncation
        ],
        "without FilterType, MIMESupport/MIMETruncation still follow BodyPreference"
    );
}

/// `mime_support: None` + `mime_truncation: None` must keep the request
/// byte-identical to the pre-Task-3 shape: Options holds exactly its
/// previous children ([BodyPreference] when bodies are fetched,
/// [FilterType] for header-only filtered rounds) and no MIMESupport /
/// MIMETruncation token appears anywhere.
#[test]
fn build_sync_request_omits_mime_elements_when_fields_none() {
    use provider_eas::wbxml::tags::{base, pages};

    // Shape 1: production default (no filter, bodies on).
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 100,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");
    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![(pages::BASE, base::BODY_PREFERENCE)],
        "Options keeps its single BodyPreference child when mime fields are None"
    );

    // Shape 2: header-only filtered round.
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");
    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![(PAGE_AIRSYNC, 0x18)],
        "Options keeps its single FilterType child when mime fields are None"
    );
}

/// `(page, token)` sequence of an Options element's children, for
/// exact-order assertions (Options-level sibling of
/// `collection_child_tokens`).
fn collection_child_tokens_of_options(options: &WbxmlElement) -> Vec<(u8, u8)> {
    options.children.iter().map(|c| (c.page, c.token)).collect()
}

// ---- Task 2 (eas-p3-commands): airsync:Supported element ----
//
// Spec anchors (docs/Exchange/mscmd.txt, [MS-ASCMD] v20250520):
// - §2.2.3.179 Supported: optional Collection child in a Sync request naming the contact/calendar
//   elements the client manages; elements NOT named are "ghosted" — a later Change omitting a
//   ghosted element PRESERVES its server-side value instead of deleting it (the pre-edit data-loss
//   hazard this task builds the foundation for). Child elements are written as empty tags — the
//   §4.24 example sends <Supported><contacts:JobTitle/><contacts:OfficeLocation/></Supported>.
// - §2.2.3.29.2 Collection (Sync) strict child order: SyncKey, CollectionId, Supported,
//   DeletesAsMoves, GetChanges, WindowSize, ConversationMode, Options, Commands — Supported sits
//   BETWEEN CollectionId and DeletesAsMoves.
// - Tokens: Supported = page 0, 0x20 (AIRSYNC_TOKENS); JobTitle = page 1, 0x28 and OfficeLocation =
//   page 1, 0x2C (CONTACTS_TOKENS), both verified in code_pages.rs.

/// Contacts code page index (code_pages.rs page 1).
const PAGE_CONTACTS: u8 = 1;
/// Contacts `JobTitle` token (page 1, 0x28 — CONTACTS_TOKENS).
const CONTACTS_JOB_TITLE: u8 = 0x28;
/// Contacts `OfficeLocation` token (page 1, 0x2C — CONTACTS_TOKENS).
const CONTACTS_OFFICE_LOCATION: u8 = 0x2C;

/// The [MS-ASCMD] §4.24 request shape: an initial Contacts Sync whose
/// Supported list names JobTitle + OfficeLocation.
fn supported_sync_request() -> SyncRequest {
    SyncRequest {
        collection_id: "2".to_string(),
        sync_key: "0".to_string(),
        class: "Contacts".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: Some(vec![
            SupportedElement {
                page: PAGE_CONTACTS,
                token: CONTACTS_JOB_TITLE,
            },
            SupportedElement {
                page: PAGE_CONTACTS,
                token: CONTACTS_OFFICE_LOCATION,
            },
        ]),
    }
}

/// With `supported` set, `<Supported>` must sit immediately after
/// CollectionId and immediately before DeletesAsMoves — the strict
/// Collection child order of [MS-ASCMD] §2.2.3.29.2 — and carry the
/// listed element tags as empty children in order (§4.24 shape).
#[test]
fn build_sync_request_emits_supported_between_collection_id_and_deletes_as_moves() {
    let collection = sync_collection_for(&supported_sync_request(), "14.0");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x20), // Supported (page 0, AIRSYNC_TOKENS)
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_GET_CHANGES),
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "Supported must sit between CollectionId and DeletesAsMoves (§2.2.3.29.2 order)"
    );

    let supported = &collection.children[2];
    assert_eq!(supported.tag_name(), "Supported");
    assert_eq!(
        supported
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>(),
        vec![
            (PAGE_CONTACTS, CONTACTS_JOB_TITLE),
            (PAGE_CONTACTS, CONTACTS_OFFICE_LOCATION),
        ],
        "Supported children are the listed element tags, in the listed order"
    );
    assert_eq!(supported.children[0].tag_name(), "JobTitle");
    assert_eq!(supported.children[1].tag_name(), "OfficeLocation");
    for child in &supported.children {
        assert_eq!(
            child.value,
            WbxmlValue::Empty,
            "Supported children are empty tags (§4.24 example shape)"
        );
    }
}

/// `supported: None` must keep the request byte-identical to the
/// pre-Supported shape: no Supported token anywhere, children exactly
/// [SyncKey, CollectionId, DeletesAsMoves, GetChanges, WindowSize].
#[test]
fn build_sync_request_omits_supported_when_none() {
    let mut req = supported_sync_request();
    req.supported = None;
    let collection = sync_collection_for(&req, "14.0");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_GET_CHANGES),
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "supported=None must be byte-identical to the pre-Supported shape"
    );
}

/// `supported: Some([])` is treated as absent — the builder must NOT
/// emit an empty `<Supported/>` (that wire form means "ghost everything"
/// per §2.2.3.179 rule 3, which no caller wants; an empty list reads as
/// rule 1: nothing ghosted, element omitted).
#[test]
fn build_sync_request_omits_supported_when_empty_vec() {
    let mut req = supported_sync_request();
    req.supported = Some(Vec::new());
    let collection = sync_collection_for(&req, "14.0");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_GET_CHANGES),
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "supported=Some([]) must omit Supported entirely (same shape as None)"
    );
}

/// A Sync request carrying Supported must survive the WBXML codec
/// losslessly — the page-1 children inside the page-0 Supported
/// container exercise the SWITCH_PAGE path in both directions.
#[test]
fn sync_request_with_supported_round_trips() {
    let tree = build_sync_request(&supported_sync_request(), "14.0");
    let back = round_trip(&tree);
    assert_eq!(
        tree, back,
        "Supported and its page-1 children must survive encode/decode"
    );
}

#[test]
fn sync_change_request_uses_spec_shape() {
    let changes = vec![
        EasChange {
            server_id: "5:12".into(),
            read: Some(true),
            starred: None,
        },
        EasChange {
            server_id: "5:13".into(),
            read: Some(false),
            starred: None,
        },
    ];
    let tree = build_sync_change_request("5", "3", &changes);
    assert_eq!((tree.page, tree.token), (PAGE_AIRSYNC, AS_SYNC));
    assert_eq!(tree.children.len(), 1);
    let collections = &tree.children[0];
    assert_eq!(
        (collections.page, collections.token),
        (PAGE_AIRSYNC, AS_COLLECTIONS)
    );
    assert_eq!(collections.children.len(), 1);
    let collection = &collections.children[0];
    assert_eq!(
        (collection.page, collection.token),
        (PAGE_AIRSYNC, AS_COLLECTION)
    );

    // Collection children: SyncKey, CollectionId, Commands — nothing else.
    assert_eq!(collection.children.len(), 3);
    let sync_key = &collection.children[0];
    assert_eq!((sync_key.page, sync_key.token), (PAGE_AIRSYNC, AS_SYNC_KEY));
    assert_eq!(text_value(sync_key).unwrap(), "3");
    let cid = &collection.children[1];
    assert_eq!((cid.page, cid.token), (PAGE_AIRSYNC, AS_COLLECTION_ID));
    assert_eq!(text_value(cid).unwrap(), "5");
    assert!(
        collection.children.iter().all(
            |c| !(c.page == PAGE_AIRSYNC && (c.token == AS_CLASS || c.token == AS_GET_CHANGES))
        ),
        "upsync Collection must not carry Class or GetChanges"
    );

    let commands = &collection.children[2];
    assert_eq!((commands.page, commands.token), (PAGE_AIRSYNC, AS_COMMANDS));
    assert_eq!(commands.children.len(), 2);
    for (i, (sid, read_str)) in [("5:12", "1"), ("5:13", "0")].iter().enumerate() {
        let change = &commands.children[i];
        assert_eq!((change.page, change.token), (PAGE_AIRSYNC, AS_CHANGE));
        assert_eq!(change.children.len(), 2);
        let server_id = &change.children[0];
        assert_eq!(
            (server_id.page, server_id.token),
            (PAGE_AIRSYNC, AS_SERVER_ID)
        );
        assert_eq!(text_value(server_id).unwrap(), *sid);
        let app_data = &change.children[1];
        assert_eq!(
            (app_data.page, app_data.token),
            (PAGE_AIRSYNC, AS_APPLICATION_DATA)
        );
        assert_eq!(app_data.children.len(), 1);
        let read = &app_data.children[0];
        // Read is the Email-page (2) token 0x15 per MS-ASWBXML §2.1.2.1.3.
        assert_eq!(
            (read.page, read.token),
            (tags::email::PAGE, tags::email::READ)
        );
        assert_eq!(text_value(read).unwrap(), *read_str);
    }
}

/// Golden-bytes test: the serialized upsync request must match this exact
/// vector (hand-derived from the WBXML spec: AirSync page 0 is the initial
/// page, the only page switch is into Email page 2 for `<Read>`).
#[test]
fn sync_change_request_matches_golden_wire_bytes() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: Some(true),
        starred: None,
    }];
    let tree = build_sync_change_request("5", "3", &changes);
    let bytes = provider_eas::wbxml::serialize_tree(&tree).expect("serialize");
    let expected: &[u8] = &[
        0x03, 0x01, 0x6A, 0x00, // WBXML header
        0x45, // Sync (0x05|0x40) on page 0 (initial page, no switch)
        0x5C, // Collections (0x1C|0x40)
        0x4F, // Collection (0x0F|0x40)
        0x4B, 0x03, 0x33, 0x00, 0x01, // SyncKey STR_I "3" + END
        0x52, 0x03, 0x35, 0x00, 0x01, // CollectionId STR_I "5" + END
        0x56, // Commands (0x16|0x40)
        0x48, // Change (0x08|0x40)
        0x4D, 0x03, 0x35, 0x3A, 0x31, 0x32, 0x00, 0x01, // ServerId STR_I "5:12" + END
        0x5D, // ApplicationData (0x1D|0x40)
        0x00, 0x02, // SWITCH_PAGE 2 (Email)
        0x55, 0x03, 0x31, 0x00, 0x01, // Read STR_I "1" + END
        0x01, // END ApplicationData
        0x01, // END Change
        0x01, // END Commands
        0x01, // END Collection
        0x01, // END Collections
        0x01, // END Sync
    ];
    assert_eq!(
        bytes, expected,
        "upsync request bytes drifted from the golden vector"
    );
}

/// The request tree survives a serialize/deserialize round trip.
#[test]
fn sync_change_request_round_trips() {
    let changes = vec![EasChange {
        server_id: "7:99".into(),
        read: Some(false),
        starred: None,
    }];
    let tree = build_sync_change_request("7", "key-1", &changes);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Task 3's builder emits only `read` — a starred-only change still
/// produces a schema-valid Change (ServerId + ApplicationData), with no
/// Read element inside ApplicationData. Task 4 extends the builder to
/// emit the Flag element for `starred`.
#[test]
fn sync_change_request_starred_only_change_has_no_read_element() {
    let changes = vec![EasChange {
        server_id: "5:1".into(),
        read: None,
        starred: Some(true),
    }];
    let tree = build_sync_change_request("5", "3", &changes);
    let change = &tree.children[0].children[0].children[2].children[0];
    assert_eq!((change.page, change.token), (PAGE_AIRSYNC, AS_CHANGE));
    let app_data = &change.children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert!(
        app_data
            .children
            .iter()
            .all(|c| !(c.page == tags::email::PAGE && c.token == tags::email::READ)),
        "no Read element when change.read is None"
    );
}

/// Task 4 golden-bytes: `starred: Some(true)` emits the full task-like
/// Flag container per Android EasSync.java:295-315 —
///   Flag (2,0x3A) > Status (2,0x3B)="2" + FlagType (2,0x3D)="FollowUp"
///     + tasks:StartDate (9,0x1E) + tasks:UtcStartDate (9,0x1F)
///     + tasks:DueDate (9,0x0C) + tasks:UtcDueDate (9,0x0D)
/// with the code page switching email(2) → tasks(9) mid-container
/// ([MS-ASWBXML] §2.1.2.1.3 / §2.1.2.1.10). The clock is pinned via
/// `build_sync_change_request_at` so the vector is deterministic:
/// now = 2026-01-01T00:00:00.000Z (epoch millis 1_767_225_600_000),
/// due = now + 7 days = 2026-01-08T00:00:00.000Z.
#[test]
fn sync_change_request_star_set_matches_golden_wire_bytes() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: None,
        starred: Some(true),
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let bytes = provider_eas::wbxml::serialize_tree(&tree).expect("serialize");
    let start = b"2026-01-01T00:00:00.000Z";
    let due = b"2026-01-08T00:00:00.000Z";
    let mut expected: Vec<u8> = vec![
        0x03, 0x01, 0x6A, 0x00, // WBXML header
        0x45, // Sync (0x05|0x40) on page 0 (initial page, no switch)
        0x5C, // Collections (0x1C|0x40)
        0x4F, // Collection (0x0F|0x40)
        0x4B, 0x03, 0x33, 0x00, 0x01, // SyncKey STR_I "3" + END
        0x52, 0x03, 0x35, 0x00, 0x01, // CollectionId STR_I "5" + END
        0x56, // Commands (0x16|0x40)
        0x48, // Change (0x08|0x40)
        0x4D, 0x03, 0x35, 0x3A, 0x31, 0x32, 0x00, 0x01, // ServerId STR_I "5:12" + END
        0x5D, // ApplicationData (0x1D|0x40)
        0x00, 0x02, // SWITCH_PAGE 2 (Email)
        0x7A, // Flag (0x3A|0x40)
        0x7B, 0x03, 0x32, 0x00, 0x01, // Status (0x3B|0x40) STR_I "2" + END
        0x7D, 0x03, // FlagType (0x3D|0x40) STR_I
    ];
    expected.extend_from_slice(b"FollowUp");
    expected.extend_from_slice(&[0x00, 0x01]); // STR_I terminator + END FlagType
    expected.extend_from_slice(&[0x00, 0x09]); // SWITCH_PAGE 9 (Tasks)
    // StartDate (0x1E|0x40), UtcStartDate (0x1F|0x40),
    // DueDate (0x0C|0x40), UtcDueDate (0x0D|0x40)
    for (tag, val) in [(0x5Eu8, start), (0x5F, start), (0x4C, due), (0x4D, due)] {
        expected.push(tag);
        expected.push(0x03); // STR_I
        expected.extend_from_slice(val);
        expected.extend_from_slice(&[0x00, 0x01]); // STR_I terminator + END
    }
    // END Flag, ApplicationData, Change, Commands, Collection, Collections, Sync
    expected.extend_from_slice(&[0x01; 7]);
    assert_eq!(
        bytes, expected,
        "star-set upsync bytes drifted from the golden vector"
    );
}

/// Task 4 golden-bytes: `starred: Some(false)` emits an EMPTY `<Flag/>`
/// element (bare token 0x3A, no WITH_CONTENT bit, no children, no page
/// switch into Tasks) — Android's `s.tag(Tags.EMAIL_FLAG)` form.
#[test]
fn sync_change_request_star_clear_matches_golden_wire_bytes() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: None,
        starred: Some(false),
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let bytes = provider_eas::wbxml::serialize_tree(&tree).expect("serialize");
    let expected: &[u8] = &[
        0x03, 0x01, 0x6A, 0x00, // WBXML header
        0x45, // Sync
        0x5C, // Collections
        0x4F, // Collection
        0x4B, 0x03, 0x33, 0x00, 0x01, // SyncKey "3"
        0x52, 0x03, 0x35, 0x00, 0x01, // CollectionId "5"
        0x56, // Commands
        0x48, // Change
        0x4D, 0x03, 0x35, 0x3A, 0x31, 0x32, 0x00, 0x01, // ServerId "5:12"
        0x5D, // ApplicationData
        0x00, 0x02, // SWITCH_PAGE 2 (Email)
        0x3A, // empty <Flag/> — bare token, no WITH_CONTENT bit, no END of its own
        0x01, // END ApplicationData
        0x01, // END Change
        0x01, // END Commands
        0x01, // END Collection
        0x01, // END Collections
        0x01, // END Sync
    ];
    assert_eq!(
        bytes, expected,
        "star-clear upsync bytes drifted from the golden vector"
    );
}

/// Structural check for a combined read+star change: Read is emitted
/// before the Flag container (Android's order), and the Flag children
/// carry the spec tokens/pages with the pinned dates.
#[test]
fn sync_change_request_read_and_star_emit_read_before_flag() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: Some(true),
        starred: Some(true),
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let app_data = &tree.children[0].children[0].children[2].children[0].children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(app_data.children.len(), 2);

    let read = &app_data.children[0];
    assert_eq!(
        (read.page, read.token),
        (tags::email::PAGE, tags::email::READ)
    );
    assert_eq!(text_value(read).unwrap(), "1");

    let flag = &app_data.children[1];
    assert_eq!(
        (flag.page, flag.token),
        (tags::email::PAGE, tags::email::FLAG)
    );
    assert_eq!(flag.children.len(), 6);
    // Status (email page 2, 0x3B) = "2"
    assert_eq!((flag.children[0].page, flag.children[0].token), (2, 0x3B));
    assert_eq!(text_value(&flag.children[0]).unwrap(), "2");
    // FlagType (email page 2, 0x3D) = "FollowUp"
    assert_eq!((flag.children[1].page, flag.children[1].token), (2, 0x3D));
    assert_eq!(text_value(&flag.children[1]).unwrap(), "FollowUp");
    // Tasks-page (9) dates: StartDate 0x1E, UtcStartDate 0x1F,
    // DueDate 0x0C, UtcDueDate 0x0D — start = now, due = now + 7 days.
    for (i, tok) in [0x1Eu8, 0x1F, 0x0C, 0x0D].iter().enumerate() {
        let el = &flag.children[2 + i];
        assert_eq!((el.page, el.token), (9, *tok));
    }
    assert_eq!(
        text_value(&flag.children[2]).unwrap(),
        "2026-01-01T00:00:00.000Z"
    );
    assert_eq!(
        text_value(&flag.children[3]).unwrap(),
        "2026-01-01T00:00:00.000Z"
    );
    assert_eq!(
        text_value(&flag.children[4]).unwrap(),
        "2026-01-08T00:00:00.000Z"
    );
    assert_eq!(
        text_value(&flag.children[5]).unwrap(),
        "2026-01-08T00:00:00.000Z"
    );
}

/// `starred: None` (with read present) emits no Flag element at all —
/// the pre-Task-4 ApplicationData shape is preserved.
#[test]
fn sync_change_request_starred_none_emits_no_flag_element() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: Some(false),
        starred: None,
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let app_data = &tree.children[0].children[0].children[2].children[0].children[1];
    assert_eq!(app_data.children.len(), 1);
    assert!(
        app_data
            .children
            .iter()
            .all(|c| !(c.page == tags::email::PAGE && c.token == tags::email::FLAG)),
        "no Flag element when change.starred is None"
    );
}

/// The std-only EAS UTC formatter: epoch, a round-midnight instant (the
/// golden-vector clock), and a leap-day + sub-second boundary.
#[test]
fn eas_datetime_utc_formats_epoch_round_midnight_and_leap_day() {
    use std::time::{Duration, UNIX_EPOCH};
    assert_eq!(
        format_eas_datetime_utc(UNIX_EPOCH),
        "1970-01-01T00:00:00.000Z"
    );
    assert_eq!(
        format_eas_datetime_utc(UNIX_EPOCH + Duration::from_millis(1_767_225_600_000)),
        "2026-01-01T00:00:00.000Z"
    );
    // 2024-02-29T23:59:59.999Z — leap day, end-of-day, max millis.
    assert_eq!(
        format_eas_datetime_utc(UNIX_EPOCH + Duration::from_millis(1_709_251_199_999)),
        "2024-02-29T23:59:59.999Z"
    );
}

/// Parse a Sync Change response: Collections > Collection carries the new
/// SyncKey and the collection Status (MS-ASSYNC §2.2.3.23).
#[test]
fn sync_change_response_parses_sync_key_and_status() {
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "2"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, "5"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                ],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "2");
    assert_eq!(outcome.status, 1);
}

/// A non-1 collection status is surfaced (the client maps it to
/// CommandStatus); an absent Status defaults to 1 (success).
#[test]
fn sync_change_response_surfaces_non_success_status() {
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "0"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "3"),
                ],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "0");
    assert_eq!(outcome.status, 3);

    // Absent Status -> success default.
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "7")],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "7");
    assert_eq!(outcome.status, 1);
}

/// A non-Sync root is a parse error, not a silent success.
#[test]
fn sync_change_response_rejects_wrong_root() {
    let response = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    assert!(parse_sync_change_response(&response).is_err());
}

/// Phase B Task 9: a Sync response to a client-Commands (upsync) request
/// MAY itself carry server-side `Commands` in the response Collection
/// ([MS-ASSYNC] §2.2.2 — the server piggybacks pending changes onto the
/// upsync response). The parser must surface them via the same parse_item
/// path the downsync uses; discarding them risks silent divergence when
/// the caller adopts the rotated key.
#[test]
fn sync_change_response_parses_piggybacked_commands() {
    let add_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:42"),
            fixture_email_app_data("Piggy Subject", "p@x", "q@y", "<p>pg</p>"),
        ],
    );
    let change_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_CHANGE,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:7"),
            WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::text(
                    tags::email::PAGE,
                    tags::email::READ,
                    "0",
                )],
            ),
        ],
    );
    // EAS Delete is a CONTAINER carrying the ServerId as a child element
    // (MS-ASCMD 2.2.3.42.2) — the spec-conformant shape; the text-leaf
    // form is only accepted by the parser as a legacy-capture fallback.
    let delete_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_DELETE,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:13")],
    );
    let commands = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COMMANDS,
        vec![add_cmd, change_cmd, delete_cmd],
    );
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "9"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, "5"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    commands,
                ],
            )],
        )],
    );

    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "9");
    assert_eq!(outcome.status, 1);

    assert_eq!(outcome.piggybacked_added.len(), 1, "one piggybacked Add");
    let added = &outcome.piggybacked_added[0];
    assert_eq!(added.server_id, "5:42");
    assert_eq!(added.subject.as_deref(), Some("Piggy Subject"));
    assert_eq!(
        added.body_html.as_deref(),
        Some("<p>pg</p>"),
        "piggybacked Add must run the full parse_item / ApplicationData path"
    );

    assert_eq!(
        outcome.piggybacked_updated.len(),
        1,
        "one piggybacked Change"
    );
    assert_eq!(outcome.piggybacked_updated[0].server_id, "5:7");
    assert_eq!(outcome.piggybacked_updated[0].read, Some(false));

    assert_eq!(
        outcome.piggybacked_deleted,
        vec!["5:13".to_string()],
        "piggybacked Delete ServerId must be surfaced"
    );
}

/// A plain upsync response with NO server-side Commands parses with empty
/// piggybacked vectors — the common case must not change shape.
#[test]
fn sync_change_response_without_commands_has_empty_piggybacked() {
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "4"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                ],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "4");
    assert!(outcome.piggybacked_added.is_empty());
    assert!(outcome.piggybacked_updated.is_empty());
    assert!(outcome.piggybacked_deleted.is_empty());
}

// ============================================================================
// M8 calendar upsync Task 2 — build_calendar_change_request golden tests
// ([MS-ASSYNC] §2.2.2 / [MS-ASWBXML] §2.1.2.1.1 page 0)
// ============================================================================

/// A minimal-but-valid [`CalendarEventWrite`] for the golden tests. The
/// subject is per-call so a crossed wire between the Add and Replace items
/// cannot hide behind equal props (the crate's fixture convention); the
/// TZI blob is a real fixed-offset UTC+8 blob via the T1 constructor.
fn calendar_write_fixture(subject: &str) -> CalendarEventWrite {
    CalendarEventWrite {
        start_time: "20260821T090000Z".to_string(),
        end_time: "20260821T100000Z".to_string(),
        all_day_event: false,
        time_zone_base64: build_fixed_offset_tzi_base64(480),
        subject: Some(subject.to_string()),
        ..Default::default()
    }
}

/// Walk to the single Collection's `Commands` element and assert the shared
/// envelope: Sync > Collections > Collection with EXACTLY [SyncKey,
/// CollectionId, Commands] as direct children — in particular NO
/// `airsync:Class` (14.0+ rejects it; CollectionId identifies the
/// collection) and NO `GetChanges` (invalid in 16.1), the same gates as the
/// email `build_sync_change_request`. Returns the Commands element.
fn assert_calendar_envelope<'a>(
    tree: &'a WbxmlElement,
    sync_key: &str,
    collection_id: &str,
) -> &'a WbxmlElement {
    assert_eq!((tree.page, tree.token), (PAGE_AIRSYNC, AS_SYNC));
    assert_eq!(tree.children.len(), 1);
    let collections = &tree.children[0];
    assert_eq!(
        (collections.page, collections.token),
        (PAGE_AIRSYNC, AS_COLLECTIONS)
    );
    assert_eq!(collections.children.len(), 1);
    let collection = &collections.children[0];
    assert_eq!(
        (collection.page, collection.token),
        (PAGE_AIRSYNC, AS_COLLECTION)
    );

    // Collection children: SyncKey, CollectionId, Commands — nothing else.
    assert_eq!(collection.children.len(), 3);
    let key = &collection.children[0];
    assert_eq!((key.page, key.token), (PAGE_AIRSYNC, AS_SYNC_KEY));
    assert_eq!(text_value(key).unwrap(), sync_key);
    let cid = &collection.children[1];
    assert_eq!((cid.page, cid.token), (PAGE_AIRSYNC, AS_COLLECTION_ID));
    assert_eq!(text_value(cid).unwrap(), collection_id);
    let commands = &collection.children[2];
    assert_eq!((commands.page, commands.token), (PAGE_AIRSYNC, AS_COMMANDS));

    // Class / GetChanges must be absent among the Collection's DIRECT
    // children (Add 0x07 lives deeper, under Commands — walking direct
    // children only keeps the assertion unambiguous).
    assert!(
        collection.children.iter().all(
            |c| !(c.page == PAGE_AIRSYNC && (c.token == AS_CLASS || c.token == AS_GET_CHANGES))
        ),
        "calendar upsync Collection must not carry Class or GetChanges"
    );
    commands
}

/// Golden test (Add): `CalendarChange::Add` emits the wire `airsync:Add`
/// container { ClientId, ApplicationData }, with the ApplicationData being
/// the T1 serializer's output unmodified ([MS-ASSYNC] §2.2.2.1).
#[test]
fn calendar_change_add_matches_golden_structure() {
    let props = calendar_write_fixture("Sprint Review");
    let client_id = new_calendar_client_id();
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Add {
            client_id: client_id.clone(),
            props: props.clone(),
        }],
        "16.1",
    );

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 1);
    let add = &commands.children[0];
    assert_eq!((add.page, add.token), (PAGE_AIRSYNC, AS_ADD));
    assert_eq!(
        add.children.len(),
        2,
        "ClientId + ApplicationData — nothing else"
    );

    let cid_el = &add.children[0];
    assert_eq!((cid_el.page, cid_el.token), (PAGE_AIRSYNC, AS_CLIENT_ID));
    assert_eq!(
        text_value(cid_el).unwrap(),
        client_id,
        "ClientId is the caller's value, verbatim"
    );

    let app_data = &add.children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(
        *app_data,
        build_calendar_application_data(&props, "16.1"),
        "ApplicationData must be the T1 serializer's output, unmodified"
    );
}

/// Golden test (Replace): OUR "Replace" vocabulary maps to the wire
/// `airsync:Change` container carrying ServerId ([MS-ASSYNC] §2.2.2 — the
/// Change command updates an existing item): { ServerId, ApplicationData }.
#[test]
fn calendar_change_replace_maps_to_wire_change_with_server_id() {
    let props = calendar_write_fixture("Moved Standup");
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Replace {
            server_id: "cal7:9".into(),
            props: props.clone(),
        }],
        "16.1",
    );

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 1);
    let change = &commands.children[0];
    assert_eq!((change.page, change.token), (PAGE_AIRSYNC, AS_CHANGE));
    assert_eq!(
        change.children.len(),
        2,
        "ServerId + ApplicationData — nothing else"
    );

    let sid = &change.children[0];
    assert_eq!((sid.page, sid.token), (PAGE_AIRSYNC, AS_SERVER_ID));
    assert_eq!(text_value(sid).unwrap(), "cal7:9");

    let app_data = &change.children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(*app_data, build_calendar_application_data(&props, "16.1"));
}

/// Golden test (Remove): OUR "Remove" vocabulary maps to the wire
/// `airsync:Delete` container whose ServerId is a CHILD element
/// ([MS-ASCMD] §2.2.3.42.2) — the server's soft-delete semantics,
/// acceptable for v1 per spec D1.
#[test]
fn calendar_change_remove_maps_to_wire_delete_with_server_id() {
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Remove {
            server_id: "cal7:12".into(),
        }],
        "16.1",
    );

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 1);
    let delete = &commands.children[0];
    assert_eq!((delete.page, delete.token), (PAGE_AIRSYNC, AS_DELETE));
    assert_eq!(
        delete.children.len(),
        1,
        "ServerId only — no ApplicationData"
    );

    let sid = &delete.children[0];
    assert_eq!((sid.page, sid.token), (PAGE_AIRSYNC, AS_SERVER_ID));
    assert_eq!(text_value(sid).unwrap(), "cal7:12");
}

/// A mixed batch [Add, Replace, Remove] preserves input order under
/// Commands, each command carrying its own identifier kind.
#[test]
fn calendar_change_mixed_batch_preserves_order() {
    let changes = vec![
        CalendarChange::Add {
            client_id: "CalAdd-fixed".into(),
            props: calendar_write_fixture("Add Subject"),
        },
        CalendarChange::Replace {
            server_id: "cal7:9".into(),
            props: calendar_write_fixture("Replace Subject"),
        },
        CalendarChange::Remove {
            server_id: "cal7:12".into(),
        },
    ];
    let tree = build_calendar_change_request("cal7", "{sk3}", &changes, "16.1");

    let commands = assert_calendar_envelope(&tree, "{sk3}", "cal7");
    assert_eq!(commands.children.len(), 3);
    let tokens: Vec<u8> = commands.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![AS_ADD, AS_CHANGE, AS_DELETE],
        "input order preserved: Add, Replace (→wire Change), Remove (→wire Delete)"
    );

    // Each command's FIRST child carries its identifier (ClientId / ServerId).
    assert_eq!(
        text_value(&commands.children[0].children[0]).unwrap(),
        "CalAdd-fixed"
    );
    assert_eq!(
        text_value(&commands.children[1].children[0]).unwrap(),
        "cal7:9"
    );
    assert_eq!(
        text_value(&commands.children[2].children[0]).unwrap(),
        "cal7:12"
    );
}

/// ClientId discipline: the builder is infallible and never synthesizes or
/// clamps — a caller-supplied value of exactly [`CLIENT_ID_MAX_LEN`] chars
/// (the [MS-ASCMD] cap; Exchange enforces it with Status 103 per task-11
/// live evidence) must be emitted verbatim, and the constructors guarantee
/// the cap by construction.
#[test]
fn calendar_change_client_id_emitted_verbatim_at_40_char_cap() {
    // "CalAdd-" + filler = exactly the cap — the longest accepted value.
    let id = format!("CalAdd-{}", "x".repeat(CLIENT_ID_MAX_LEN - "CalAdd-".len()));
    assert_eq!(id.len(), CLIENT_ID_MAX_LEN);
    let tree = build_calendar_change_request(
        "cal7",
        "{sk3}",
        &[CalendarChange::Add {
            client_id: id.clone(),
            props: calendar_write_fixture("Cap Test"),
        }],
        "16.1",
    );
    let cid_el = &tree.children[0].children[0].children[2].children[0].children[0];
    assert_eq!((cid_el.page, cid_el.token), (PAGE_AIRSYNC, AS_CLIENT_ID));
    assert_eq!(
        text_value(cid_el).unwrap(),
        id,
        "builder emits the caller's ClientId verbatim — it never synthesizes or clamps"
    );

    // The constructors guarantee the cap: the fixed "CalAdd-" prefix
    // (7 + 32-hex uuid = 39 ≤ 40), and the shared clamp still yields
    // exactly CLIENT_ID_MAX_LEN for an over-long prefix input.
    let synthesized = new_calendar_client_id();
    assert!(synthesized.len() <= CLIENT_ID_MAX_LEN);
    assert!(synthesized.starts_with("CalAdd-"));
    let clamped = new_send_client_id(&"P".repeat(100));
    assert_eq!(clamped.len(), CLIENT_ID_MAX_LEN);
}

/// The mixed calendar upsync request survives a WBXML serialize/deserialize
/// round trip — the page-4 ApplicationData children exercise the
/// SWITCH_PAGE path in both directions.
#[test]
fn calendar_change_request_round_trips() {
    let changes = vec![
        CalendarChange::Add {
            client_id: new_calendar_client_id(),
            props: calendar_write_fixture("RT Add"),
        },
        CalendarChange::Replace {
            server_id: "cal7:9".into(),
            props: calendar_write_fixture("RT Replace"),
        },
        CalendarChange::Remove {
            server_id: "cal7:12".into(),
        },
    ];
    let tree = build_calendar_change_request("cal7", "{sk3}", &changes, "16.1");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

// ============================================================================
// M8 calendar upsync Task 3 — parse_sync_change_response: Responses
// Add-ack + per-item Status parsing
// ([MS-ASCMD] §2.2.3.154 Responses; §2.2.3.7.2 Add (Sync); §2.2.3.24 Change;
// §2.2.3.42.2 Delete; §2.2.3.177.17 Status (Sync) — docs/Exchange/mscmd.txt)
// ============================================================================

/// Build a `Responses` Add item in the §4.5.3.2 example wire order
/// (ClientId, ServerId?, Status). `server_id: None` emits no ServerId
/// element — the shape of a FAILED add (the server only assigns the id on
/// success).
fn response_add_item(client_id: &str, status: &str, server_id: Option<&str>) -> WbxmlElement {
    let mut children = vec![WbxmlElement::text(PAGE_AIRSYNC, AS_CLIENT_ID, client_id)];
    if let Some(sid) = server_id {
        children.push(WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, sid));
    }
    children.push(WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status));
    WbxmlElement::container(PAGE_AIRSYNC, AS_ADD, children)
}

/// Build a `Responses` Change/Delete item ([MS-ASCMD] §2.2.3.24 /
/// §2.2.3.42.2): { ServerId, Status }. The command token is supplied by the
/// caller (`AS_CHANGE` / `AS_DELETE`).
fn response_status_item(command_token: u8, server_id: &str, status: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        command_token,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, server_id),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status),
        ],
    )
}

/// Wrap response Collection children in the full
/// `Sync > Collections > Collection` upsync-response envelope, so tests
/// route through the real parser entry instead of raw index chains.
fn upsync_response(collection_children: Vec<WbxmlElement>) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                collection_children,
            )],
        )],
    )
}

/// Fixture A: a successful calendar Add — Responses > Add { ClientId
/// "CalAdd-abc", ServerId "5:7", Status 1 } (§4.5.3.2 shape) plus the
/// rotated SyncKey and collection Status 1. Asserted directly AND after a
/// WBXML round trip.
#[test]
fn sync_change_response_parses_add_ack() {
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{rot-1}"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![response_add_item("CalAdd-abc", "1", Some("5:7"))],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");
    assert_eq!(outcome.new_key, "{rot-1}", "rotated key captured");
    assert_eq!(outcome.status, 1);
    assert_eq!(outcome.add_acks.len(), 1, "exactly one Add ack");
    let ack = &outcome.add_acks[0];
    assert_eq!(ack.client_id, "CalAdd-abc");
    assert_eq!(ack.status, 1);
    assert_eq!(ack.server_id.as_deref(), Some("5:7"));
    assert!(ack.success(), "status 1 ack must read as success");
    assert!(outcome.item_statuses.is_empty(), "no Change/Delete items");
    assert!(
        !outcome.has_piggybacked(),
        "no piggybacked Commands in this fixture"
    );

    // WBXML round trip: the ack must survive real encode/decode bytes, not
    // just the in-memory tree (locks the AS_RESPONSES 0x06 / AS_CLIENT_ID
    // 0x0C tokens through the codec's page handling).
    let back = round_trip(&tree);
    let outcome_rt = parse_sync_change_response(&back).expect("parse after round trip");
    assert_eq!(outcome_rt.add_acks, outcome.add_acks);
    assert_eq!(outcome_rt.new_key, "{rot-1}");
}

/// Fixture B: a FAILED Add (Status 6, no ServerId) plus per-item statuses
/// for a Change and a Delete. Status 6 per [MS-ASCMD] §2.2.3.177.17 is
/// "Error in client/server conversion" — the client sent a malformed or
/// invalid item; item-scoped, NOT transient ("stop sending the item").
#[test]
fn sync_change_response_parses_failed_add_and_change_delete_item_statuses() {
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{rot-2}"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![
                response_add_item("CalAdd-bad", "6", None),
                response_status_item(AS_CHANGE, "5:8", "1"),
                response_status_item(AS_DELETE, "5:9", "1"),
            ],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");

    // The failed Add: ack present, no ServerId, success() false.
    assert_eq!(outcome.add_acks.len(), 1);
    let ack = &outcome.add_acks[0];
    assert_eq!(ack.client_id, "CalAdd-bad");
    assert_eq!(ack.status, 6);
    assert_eq!(ack.server_id, None, "failed Add carries no ServerId");
    assert!(!ack.success());

    // Per-item statuses: Change and Delete both surface with their kinds.
    assert_eq!(outcome.item_statuses.len(), 2);
    let change = &outcome.item_statuses[0];
    assert_eq!(change.server_id, "5:8");
    assert_eq!(change.status, 1);
    assert_eq!(change.kind, ResponseItemKind::Change);
    assert!(change.success());
    let delete = &outcome.item_statuses[1];
    assert_eq!(delete.server_id, "5:9");
    assert_eq!(delete.status, 1);
    assert_eq!(delete.kind, ResponseItemKind::Delete);
    assert!(delete.success());
}

/// Fixture C (email-shaped regression): a response with NO Responses element
/// — the common email upsync shape — must parse with both new vectors empty
/// and everything else unchanged.
#[test]
fn sync_change_response_without_responses_has_empty_ack_vectors() {
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "4"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");
    assert_eq!(outcome.new_key, "4");
    assert_eq!(outcome.status, 1);
    assert!(outcome.add_acks.is_empty());
    assert!(outcome.item_statuses.is_empty());
    assert!(outcome.piggybacked_added.is_empty());
    assert!(outcome.piggybacked_updated.is_empty());
    assert!(outcome.piggybacked_deleted.is_empty());
}

/// Fixture D: the [MS-ASSYNC] §2.2.2 piggyback case AND Responses in ONE
/// response — a Commands block (server-side changes) alongside a Responses
/// block (acks for the client's commands). Both must parse; neither may
/// starve the other.
#[test]
fn sync_change_response_parses_piggybacked_commands_and_responses_together() {
    let piggybacked_add = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:42"),
            fixture_email_app_data("Piggy + Acks", "p@x", "q@y", "<p>both</p>"),
        ],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![piggybacked_add]);
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "9"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        commands,
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![
                response_add_item("CalAdd-both", "1", Some("5:77")),
                response_status_item(AS_CHANGE, "5:78", "8"),
            ],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");

    // Piggybacked Commands still parse via the email path.
    assert_eq!(outcome.new_key, "9");
    assert!(outcome.has_piggybacked());
    assert_eq!(outcome.piggybacked_added.len(), 1);
    assert_eq!(outcome.piggybacked_added[0].server_id, "5:42");
    assert_eq!(
        outcome.piggybacked_added[0].subject.as_deref(),
        Some("Piggy + Acks")
    );

    // Responses parse alongside them.
    assert_eq!(outcome.add_acks.len(), 1);
    assert_eq!(outcome.add_acks[0].server_id.as_deref(), Some("5:77"));
    assert_eq!(outcome.item_statuses.len(), 1);
    // Status 8 = "Object not found" ([MS-ASCMD] §2.2.3.177.17) — the
    // ServerId is no longer valid on the server; NOT a success.
    assert_eq!(outcome.item_statuses[0].status, 8);
    assert!(!outcome.item_statuses[0].success());
}

/// Malformed-shape policy (permissive, like the rest of the file): an Add
/// without a ClientId warns and is skipped; a Change/Delete without a
/// ServerId warns and is skipped; unknown Response kinds (`Fetch`,
/// [MS-ASCMD] §2.2.3.67.2) are debug-skipped. The well-formed siblings in
/// the same Responses block still parse.
#[test]
fn sync_change_response_skips_malformed_and_unknown_response_items() {
    let fetch = WbxmlElement::container(
        PAGE_AIRSYNC,
        tags::airsync::FETCH,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "1:14"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        ],
    );
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "5"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![
                // Add with NO ClientId — uncorrelatable, skipped.
                WbxmlElement::container(
                    PAGE_AIRSYNC,
                    AS_ADD,
                    vec![
                        WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:1"),
                        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    ],
                ),
                // Unknown kind: a Fetch response (§4.5.2.2 shape).
                fetch,
                // Change with NO ServerId — skipped.
                WbxmlElement::container(
                    PAGE_AIRSYNC,
                    AS_CHANGE,
                    vec![WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1")],
                ),
                // The well-formed siblings.
                response_add_item("CalAdd-ok", "1", Some("5:2")),
                response_status_item(AS_DELETE, "5:3", "1"),
            ],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");
    assert_eq!(
        outcome.add_acks,
        vec![CalendarAddAck {
            client_id: "CalAdd-ok".to_string(),
            status: 1,
            server_id: Some("5:2".to_string()),
        }],
        "only the Add carrying a ClientId is acked"
    );
    assert_eq!(
        outcome.item_statuses,
        vec![CalendarItemStatus {
            server_id: "5:3".to_string(),
            status: 1,
            kind: ResponseItemKind::Delete,
        }],
        "only the Delete carrying a ServerId surfaces"
    );
}
