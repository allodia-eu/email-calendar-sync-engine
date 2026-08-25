// SPDX-License-Identifier: MPL-2.0
//! parse_application_data: Email-class ApplicationData fields (body, flag, attachments,
//! conversation id, draft).

use super::*;

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
    let raw_mime = "Received: from example.com\r\nFrom: Chris Gray <chris@example.com>\r\nSubject: opaque s + e\r\n\r\nbody";
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
