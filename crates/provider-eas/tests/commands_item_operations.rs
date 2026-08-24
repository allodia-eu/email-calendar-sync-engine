// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::{tests_common::*, *};

#[test]
fn item_operations_request_attachment_round_trips() {
    let req = ItemOperationsFetchRequest {
        server_id: "srv-1".to_string(),
        collection_id: "col-1".to_string(),
        file_reference: Some("fileref-abc".to_string()),
        long_id: None,
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

#[test]
fn item_operations_response_parses_attachment_data() {
    use tags::item_operations as io;
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::RESPONSE,
            vec![WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::FETCH,
                vec![
                    WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
                    WbxmlElement::container(
                        PAGE_ITEM_OPS,
                        io::PROPERTIES,
                        vec![
                            WbxmlElement::text(PAGE_ITEM_OPS, io::DATA, "QkFTRTY0REFUQQ=="),
                            WbxmlElement::text(pages::BASE, tags::base::CONTENT_TYPE, "image/png"),
                        ],
                    ),
                ],
            )],
        )],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.content_type.as_deref(), Some("image/png"));
    assert_eq!(parsed.data.as_deref(), Some("QkFTRTY0REFUQQ=="));
}

// ---- ItemOperations spec-shape tests (MS-ASWBXML §2.1.2.1.21 page 20) ----
//
// Page-20 table: ItemOperations=0x05, Fetch=0x06, Store=0x07, Options=0x08,
// Range=0x09, Total=0x0A, Properties=0x0B, Data=0x0C, Status=0x0D,
// Response=0x0E, Version=0x0F, Schema=0x10, Part=0x11.
// Inside a Fetch the id elements are NOT page-20 tokens: CollectionId and
// ServerId are AirSync-page (0) tokens 0x12 / 0x0D, and FileReference is
// AirSyncBase-page (17) token 0x11 — per MS-ASCMD §2.2.1.10 (schema 6.23:
// airsync:ServerId / airsync:CollectionId / airsyncbase:FileReference).
// The response ContentType is airsyncbase:ContentType (page 17, 0x17) per
// MS-ASCMD §2.2.3.139.2.

/// Attachment-fetch request: ItemOperations(20,0x05) > Fetch(20,0x06) >
/// { Store(20,0x07)="Mailbox", FileReference(17,0x11) } — and nothing else.
#[test]
fn item_operations_request_attachment_uses_spec_pages_and_tokens() {
    let req = ItemOperationsFetchRequest {
        server_id: String::new(),
        collection_id: String::new(),
        file_reference: Some("fileref-xyz".to_string()),
        long_id: None,
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    assert_eq!((tree.page, tree.token), (PAGE_ITEM_OPS, 0x05)); // ItemOperations
    assert_eq!(tree.children.len(), 1);
    let fetch = &tree.children[0];
    assert_eq!((fetch.page, fetch.token), (PAGE_ITEM_OPS, 0x06)); // Fetch
    assert_eq!(fetch.children.len(), 2);

    let store = &fetch.children[0];
    assert_eq!((store.page, store.token), (PAGE_ITEM_OPS, 0x07)); // Store
    assert_eq!(text_value(store).unwrap(), "Mailbox");

    let file_ref = &fetch.children[1];
    assert_eq!(
        (file_ref.page, file_ref.token),
        (pages::BASE, tags::base::FILE_REFERENCE),
        "FileReference must be the AirSyncBase-page (17) token 0x11"
    );
    assert_eq!(text_value(file_ref).unwrap(), "fileref-xyz");
}

/// Body/item-fetch request: ItemOperations(20,0x05) > Fetch(20,0x06) >
/// { Store(20,0x07), airsync:CollectionId(0,0x12), airsync:ServerId(0,0x0D),
///   Options(20,0x08) > airsyncbase:BodyPreference(17,0x05) > Type(17,0x06)"2" }.
/// The BodyPreference is required: without it Exchange 2019 returns the
/// Body element metadata-only (no Data child) — live evidence 2026-08-02.
#[test]
fn item_operations_request_body_fetch_uses_airsync_id_tokens() {
    let req = ItemOperationsFetchRequest {
        server_id: "7:3".to_string(),
        collection_id: "7".to_string(),
        file_reference: None,
        long_id: None,
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    assert_eq!((tree.page, tree.token), (PAGE_ITEM_OPS, 0x05));
    let fetch = &tree.children[0];
    assert_eq!((fetch.page, fetch.token), (PAGE_ITEM_OPS, 0x06));
    assert_eq!(fetch.children.len(), 4);

    let store = &fetch.children[0];
    assert_eq!((store.page, store.token), (PAGE_ITEM_OPS, 0x07));

    let collection_id = &fetch.children[1];
    assert_eq!(
        (collection_id.page, collection_id.token),
        (PAGE_AIRSYNC, 0x12),
        "CollectionId must be the AirSync-page (0) token 0x12"
    );
    assert_eq!(text_value(collection_id).unwrap(), "7");

    let server_id = &fetch.children[2];
    assert_eq!(
        (server_id.page, server_id.token),
        (PAGE_AIRSYNC, 0x0D),
        "ServerId must be the AirSync-page (0) token 0x0D"
    );
    assert_eq!(text_value(server_id).unwrap(), "7:3");

    let options = &fetch.children[3];
    assert_eq!((options.page, options.token), (PAGE_ITEM_OPS, 0x08)); // Options
    let body_pref = &options.children[0];
    assert_eq!((body_pref.page, body_pref.token), (pages::BASE, 0x05)); // BodyPreference
    let bp_type = &body_pref.children[0];
    assert_eq!((bp_type.page, bp_type.token), (pages::BASE, 0x06)); // Type
    assert_eq!(text_value(bp_type).unwrap(), "2"); // HTML
}

/// Fetch by search:LongId ([MS-ASCMD] §4.10.3.3 — "Fetching an Email Item
/// with a LongId"): ItemOperations(20,0x05) > Fetch(20,0x06) >
/// { Store(20,0x07)="Mailbox", search:LongId(15,0x18),
///   Options(20,0x08) > airsyncbase:BodyPreference(17,0x05) >
///   Type(17,0x06)="2" }. The LongId here is the SEARCH-page (15) token
/// 0x18 ([MS-ASWBXML] §2.1.2.1.16; §2.2.3.98.1) — NOT a page-20 token and
/// not the compose-page LongId (21,0x0E). No CollectionId / ServerId /
/// FileReference is emitted in this form (§2.2.3.98.1: LongId replaces
/// them). BodyPreference Type 2 (HTML) matches the collection/server-id
/// body fetch — the spec example uses Type 1, but without an explicit
/// HTML preference Exchange answers metadata-only for HTML-bodied mail
/// (live evidence 2026-08-02).
#[test]
fn item_operations_request_long_id_fetch_wire_shape() {
    let req = ItemOperationsFetchRequest {
        server_id: String::new(),
        collection_id: String::new(),
        file_reference: None,
        long_id: Some("RgAAAACYWCHnyBZ%2fTq8bujFmR1EPBwBzyWfENpc".to_string()),
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    assert_eq!((tree.page, tree.token), (PAGE_ITEM_OPS, 0x05)); // ItemOperations
    assert_eq!(tree.children.len(), 1);
    let fetch = &tree.children[0];
    assert_eq!((fetch.page, fetch.token), (PAGE_ITEM_OPS, 0x06)); // Fetch
    assert_eq!(fetch.children.len(), 3);

    let store = &fetch.children[0];
    assert_eq!((store.page, store.token), (PAGE_ITEM_OPS, 0x07)); // Store
    assert_eq!(text_value(store).unwrap(), "Mailbox");

    let long_id = &fetch.children[1];
    assert_eq!(
        (long_id.page, long_id.token),
        (tags::search::PAGE, tags::search::LONG_ID),
        "LongId must be the Search-page (15) token 0x18"
    );
    assert_eq!(
        text_value(long_id).unwrap(),
        "RgAAAACYWCHnyBZ%2fTq8bujFmR1EPBwBzyWfENpc"
    );

    let options = &fetch.children[2];
    assert_eq!((options.page, options.token), (PAGE_ITEM_OPS, 0x08)); // Options
    assert_eq!(options.children.len(), 1);
    let body_pref = &options.children[0];
    assert_eq!(
        (body_pref.page, body_pref.token),
        (pages::BASE, tags::base::BODY_PREFERENCE),
    );
    let bp_type = &body_pref.children[0];
    assert_eq!(
        (bp_type.page, bp_type.token),
        (pages::BASE, tags::base::TYPE)
    );
    assert_eq!(text_value(bp_type).unwrap(), "2"); // HTML
}

#[test]
fn item_operations_request_long_id_fetch_round_trips() {
    let req = ItemOperationsFetchRequest {
        server_id: String::new(),
        collection_id: String::new(),
        file_reference: None,
        long_id: Some("RgAAAA==".to_string()),
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Precedence guard: when both `file_reference` and `long_id` are set the
/// attachment-fetch shape wins (documented precedence: file_reference >
/// long_id > collection/server-id). Regression guard for the pre-Task-5
/// attachment wire shape.
#[test]
fn item_operations_request_file_reference_beats_long_id() {
    let req = ItemOperationsFetchRequest {
        server_id: String::new(),
        collection_id: String::new(),
        file_reference: Some("fileref-1".to_string()),
        long_id: Some("RgAAAA==".to_string()),
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    let fetch = &tree.children[0];
    assert_eq!(
        fetch.children.len(),
        2,
        "attachment shape: Store + FileReference only"
    );
    assert_eq!(
        (fetch.children[1].page, fetch.children[1].token),
        (pages::BASE, tags::base::FILE_REFERENCE)
    );
    assert_eq!(text_value(&fetch.children[1]).unwrap(), "fileref-1");
}

// ---- Task 3 (eas-p2-polish): MIMESupport + Type-4 (MIME) item fetch ----
//
// Spec anchors (docs/Exchange/mscmd.txt, [MS-ASCMD] v20250520):
// - §4.10.2.1 example (fetching a MIME message): Options carries
//   `<airsync:MIMESupport>1</airsync:MIMESupport>` BEFORE
//   `<airsyncbase:BodyPreference><airsyncbase:Type>4</airsyncbase:Type>…`.
// - §2.2.3.110.1 MIMESupport (ItemOperations): 0 = never send MIME, 1 = S/MIME messages only, 2 =
//   all messages. This builder emits level 2 when `mime` is set: the caller asked for raw MIME, so
//   the server must send it for ALL messages, not just S/MIME ones.
// - MIMESupport is an AirSync-page (0) token 0x22, also inside ItemOperations Options (verified in
//   code_pages.rs AIRSYNC_TOKENS).

/// MIME item fetch ([MS-ASCMD] §4.10.2.1): ItemOperations(20,0x05) >
/// Fetch(20,0x06) > { Store(20,0x07)="Mailbox", airsync:CollectionId(0,0x12),
///   airsync:ServerId(0,0x0D), Options(20,0x08) >
///   [ airsync:MIMESupport(0,0x22)="2",
///     airsyncbase:BodyPreference(17,0x05) > Type(17,0x06)="4" ] }.
/// MIMESupport MUST precede BodyPreference (spec example order).
#[test]
fn item_operations_request_mime_fetch_wire_shape() {
    let req = ItemOperationsFetchRequest {
        server_id: "17:11".to_string(),
        collection_id: "17".to_string(),
        file_reference: None,
        long_id: None,
        mime: true,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    assert_eq!((tree.page, tree.token), (PAGE_ITEM_OPS, 0x05)); // ItemOperations
    assert_eq!(tree.children.len(), 1);
    let fetch = &tree.children[0];
    assert_eq!((fetch.page, fetch.token), (PAGE_ITEM_OPS, 0x06)); // Fetch
    assert_eq!(fetch.children.len(), 4);

    let store = &fetch.children[0];
    assert_eq!((store.page, store.token), (PAGE_ITEM_OPS, 0x07)); // Store
    assert_eq!(text_value(store).unwrap(), "Mailbox");

    let collection_id = &fetch.children[1];
    assert_eq!(
        (collection_id.page, collection_id.token),
        (PAGE_AIRSYNC, 0x12)
    );
    assert_eq!(text_value(collection_id).unwrap(), "17");

    let server_id = &fetch.children[2];
    assert_eq!((server_id.page, server_id.token), (PAGE_AIRSYNC, 0x0D));
    assert_eq!(text_value(server_id).unwrap(), "17:11");

    let options = &fetch.children[3];
    assert_eq!((options.page, options.token), (PAGE_ITEM_OPS, 0x08)); // Options
    assert_eq!(
        options.children.len(),
        2,
        "MIME fetch Options must hold exactly MIMESupport + BodyPreference, got {:?}",
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>()
    );

    // MIMESupport FIRST (§4.10.2.1 example order), AirSync page 0 token 0x22.
    let mime_support = &options.children[0];
    assert_eq!(
        (mime_support.page, mime_support.token),
        (PAGE_AIRSYNC, 0x22),
        "MIMESupport must be the AirSync-page (0) token 0x22, BEFORE BodyPreference"
    );
    assert_eq!(mime_support.tag_name(), "MIMESupport");
    assert_eq!(
        text_value(mime_support).unwrap(),
        "2",
        "level 2 = send MIME for all messages (§2.2.3.110.3)"
    );

    // BodyPreference with Type 4 (MIME BLOB) second.
    let body_pref = &options.children[1];
    assert_eq!(
        (body_pref.page, body_pref.token),
        (pages::BASE, tags::base::BODY_PREFERENCE),
        "BodyPreference must follow MIMESupport inside Options"
    );
    assert_eq!(body_pref.children.len(), 1);
    let bp_type = &body_pref.children[0];
    assert_eq!(
        (bp_type.page, bp_type.token),
        (pages::BASE, tags::base::TYPE)
    );
    assert_eq!(text_value(bp_type).unwrap(), "4", "Type 4 = MIME BLOB");
}

/// MIME fetch round-trips through the WBXML codec losslessly.
#[test]
fn item_operations_request_mime_fetch_round_trips() {
    let req = ItemOperationsFetchRequest {
        server_id: "17:11".to_string(),
        collection_id: "17".to_string(),
        file_reference: None,
        long_id: None,
        mime: true,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Regression guard: `mime: false` (the default) keeps the pre-Task-3
/// shape exactly — Options holds a single BodyPreference(Type 2), and no
/// MIMESupport element appears anywhere in the request.
#[test]
fn item_operations_request_default_fetch_has_no_mime_support() {
    let req = ItemOperationsFetchRequest {
        server_id: "7:3".to_string(),
        collection_id: "7".to_string(),
        file_reference: None,
        long_id: None,
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    let fetch = &tree.children[0];
    assert_eq!(fetch.children.len(), 4); // Store, CollectionId, ServerId, Options
    let options = &fetch.children[3];
    assert_eq!((options.page, options.token), (PAGE_ITEM_OPS, 0x08));
    assert_eq!(
        options.children.len(),
        1,
        "default fetch Options keeps its single BodyPreference child, got {:?}",
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>()
    );
    let body_pref = &options.children[0];
    assert_eq!(
        (body_pref.page, body_pref.token),
        (pages::BASE, tags::base::BODY_PREFERENCE)
    );
    assert_eq!(text_value(&body_pref.children[0]).unwrap(), "2"); // HTML, unchanged

    // No MIMESupport token anywhere in the tree.
    fn contains_mime_support(el: &WbxmlElement) -> bool {
        if el.page == PAGE_AIRSYNC && el.token == 0x22 {
            return true;
        }
        el.children.iter().any(contains_mime_support)
    }
    assert!(
        !contains_mime_support(&tree),
        "MIMESupport must NOT appear when mime=false"
    );
}

/// Spec-shaped response parse: ItemOperations(20,0x05) > Status(20,0x0D)"1"
/// + Response(20,0x0E) > Fetch(20,0x06) > { Status(20,0x0D)"1", Properties(20,0x0B) > {
///   Data(20,0x0C), airsyncbase:ContentType(17,0x17) } }. Regression guard: the old constants read
///   Response as 0x08 (Options), Status as 0x0A (Total), Properties as 0x0F (Version), Data as 0x10
///   (Schema), ContentType as page-20 0x12 (EmptyFolderContents) — so every live response parsed to
///   status 0 / no data.
#[test]
fn item_operations_response_parses_spec_shaped_response() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05, // ItemOperations
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // top-level Status
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E, // Response
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06, // Fetch
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // fetch Status
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B, // Properties
                            vec![
                                WbxmlElement::text(PAGE_ITEM_OPS, 0x0C, "aGVsbG8="), // Data
                                WbxmlElement::text(pages::BASE, 0x17, "text/plain"), /* airsyncbase:ContentType */
                            ],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some("aGVsbG8="));
    assert_eq!(parsed.content_type.as_deref(), Some("text/plain"));
}

/// An item/body fetch returns the payload as Properties >
/// airsyncbase:Body(17,0x0A) > { Type(17,0x06), Data(17,0x0B) } rather
/// than a page-20 Data element. The parser must surface Body.Data as
/// `result.data` and derive content_type from the Body Type when the
/// server omits airsyncbase:ContentType. Live evidence 2026-08-02:
/// Exchange 2019 answers exactly this shape.
#[test]
fn item_operations_response_parses_body_fetch_payload() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B,
                            vec![WbxmlElement::container(
                                pages::BASE,
                                0x0A, // airsyncbase:Body
                                vec![
                                    WbxmlElement::text(pages::BASE, 0x06, "2"), // Type=HTML
                                    WbxmlElement::text(pages::BASE, 0x0B, "<p>hi</p>"), // Data
                                ],
                            )],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some("<p>hi</p>"));
    assert_eq!(parsed.content_type.as_deref(), Some("text/html"));
}

/// Task 3: a MIME fetch answer carries Properties > airsyncbase:Body with
/// Type=4 (MIME BLOB, [MS-ASCMD] §4.10.2.2 response example). The parser
/// must surface Body.Data as `result.data` and, when the server omits
/// airsyncbase:ContentType, fall back to `message/rfc822` — mirroring the
/// Type 1/2 → text/plain|html fallbacks.
#[test]
fn item_operations_response_type_4_body_falls_back_to_message_rfc822() {
    let raw_mime = "From: a@b\r\nSubject: opaque s + e\r\n\r\nbody";
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B,
                            vec![WbxmlElement::container(
                                pages::BASE,
                                0x0A, // airsyncbase:Body
                                vec![
                                    WbxmlElement::text(pages::BASE, 0x06, "4"), // Type = MIME
                                    WbxmlElement::text(pages::BASE, 0x0C, "13813"), /* EstimatedDataSize */
                                    WbxmlElement::text(pages::BASE, 0x0B, raw_mime), // Data
                                ],
                            )],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some(raw_mime));
    assert_eq!(
        parsed.content_type.as_deref(),
        Some("message/rfc822"),
        "Type-4 body without an explicit ContentType must read as message/rfc822"
    );
}

/// A fetch-level failure status (e.g. 3 = not found) must override the
/// top-level success Status, matching the Sync parser's "more specific
/// wins" rule.
#[test]
fn item_operations_response_fetch_status_overrides_top_level() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // top-level OK
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "3")], // fetch failed
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 3);
    assert!(parsed.data.is_none());
}

// ---- ItemOperations EmptyFolderContents ([MS-ASCMD] §4.14.4) ----

/// Request shape per [MS-ASCMD] §4.14.4.1 plus the optional
/// `Options>DeleteSubFolders` form ([MS-ASWBXML] §2.1.2.1.21, code
/// page 20):
/// ```text
/// ItemOperations (20,0x05) > EmptyFolderContents (20,0x12) >
///   airsync:CollectionId (0,0x12) = "15",
///   Options (20,0x08) > DeleteSubFolders (20,0x13) — empty element
/// ```
/// Exact child sequence: CollectionId first, Options second.
#[test]
fn empty_folder_contents_request_wire_shape_with_delete_sub_folders() {
    use tags::item_operations as io;
    let req = EmptyFolderContentsRequest {
        collection_id: "15".to_string(),
        delete_sub_folders: true,
    };
    let tree = build_empty_folder_contents_request(&req);
    assert_eq!(
        (tree.page, tree.token),
        (PAGE_ITEM_OPS, io::ITEM_OPERATIONS)
    );
    assert_eq!(tree.children.len(), 1);
    let efc = &tree.children[0];
    assert_eq!(
        (efc.page, efc.token),
        (PAGE_ITEM_OPS, io::EMPTY_FOLDER_CONTENTS)
    );
    assert_eq!(efc.children.len(), 2);
    let cid = &efc.children[0];
    assert_eq!((cid.page, cid.token), (PAGE_AIRSYNC, AS_COLLECTION_ID));
    assert_eq!(text_value(cid).expect("collection id text"), "15");
    let options = &efc.children[1];
    assert_eq!((options.page, options.token), (PAGE_ITEM_OPS, io::OPTIONS));
    assert_eq!(options.children.len(), 1);
    let dsf = &options.children[0];
    assert_eq!(
        (dsf.page, dsf.token),
        (PAGE_ITEM_OPS, io::DELETE_SUB_FOLDERS)
    );
    assert!(dsf.children.is_empty());
    assert!(matches!(dsf.value, WbxmlValue::Empty));
}

/// When `delete_sub_folders` is false the whole Options element is
/// omitted (the server default keeps subfolders) — EmptyFolderContents
/// carries only the CollectionId child, matching the §4.14.4.1 example
/// exactly.
#[test]
fn empty_folder_contents_request_omits_options_without_delete_sub_folders() {
    use tags::item_operations as io;
    let req = EmptyFolderContentsRequest {
        collection_id: "15".to_string(),
        delete_sub_folders: false,
    };
    let tree = build_empty_folder_contents_request(&req);
    assert_eq!(
        (tree.page, tree.token),
        (PAGE_ITEM_OPS, io::ITEM_OPERATIONS)
    );
    let efc = &tree.children[0];
    assert_eq!(
        (efc.page, efc.token),
        (PAGE_ITEM_OPS, io::EMPTY_FOLDER_CONTENTS)
    );
    assert_eq!(efc.children.len(), 1);
    let cid = &efc.children[0];
    assert_eq!((cid.page, cid.token), (PAGE_AIRSYNC, AS_COLLECTION_ID));
    assert_eq!(text_value(cid).expect("collection id text"), "15");
}

#[test]
fn empty_folder_contents_request_round_trips() {
    for delete_sub_folders in [false, true] {
        let req = EmptyFolderContentsRequest {
            collection_id: "15".to_string(),
            delete_sub_folders,
        };
        let tree = build_empty_folder_contents_request(&req);
        let back = round_trip(&tree);
        assert_eq!(tree, back);
    }
}

/// Response shape per [MS-ASCMD] §4.14.4.2:
/// ```text
/// ItemOperations (20,0x05) > Status (20,0x0D) = 1,
///   Response (20,0x0E) > EmptyFolderContents (20,0x12) >
///     Status (20,0x0D) = 1, airsync:CollectionId (0,0x12) = "15"
/// ```
/// Both status levels surface; the CollectionId echo confirms which
/// folder was emptied.
#[test]
fn empty_folder_contents_response_parses_spec_shape() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    io::EMPTY_FOLDER_CONTENTS,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
                        WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, "15"),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_empty_folder_contents_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.empty_status, Some(1));
    assert_eq!(parsed.collection_id, Some("15".to_string()));
}

/// Nested-Status rule (mirrors the ItemOperations fetch parser and the
/// Settings family): the EmptyFolderContents-level Status overrides the
/// top-level one when both are present (more specific wins), while both
/// remain surfaced — the specific one via `empty_status`.
#[test]
fn empty_folder_contents_nested_status_overrides_top_level() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    io::EMPTY_FOLDER_CONTENTS,
                    vec![WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "4")],
                )],
            ),
        ],
    );
    let parsed = parse_empty_folder_contents_response(&tree).expect("parse");
    assert_eq!(parsed.empty_status, Some(4));
    assert_eq!(parsed.status, 4); // more specific wins
    assert_eq!(parsed.collection_id, None);
}

/// A command-level rejection — top-level Status only (e.g. 143 device
/// not provisioned), no Response element — surfaces on `status`;
/// `empty_status` and `collection_id` stay None.
#[test]
fn empty_folder_contents_response_command_level_error() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "143")],
    );
    let parsed = parse_empty_folder_contents_response(&tree).expect("parse");
    assert_eq!(parsed.status, 143);
    assert_eq!(parsed.empty_status, None);
    assert_eq!(parsed.collection_id, None);
}

/// Absent Status elements default `status` to 1 (success), mirroring
/// the GetItemEstimate/Settings family pattern; `empty_status` stays
/// None.
#[test]
fn empty_folder_contents_response_defaults_status_when_absent() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(PAGE_ITEM_OPS, io::ITEM_OPERATIONS, vec![]);
    let parsed = parse_empty_folder_contents_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.empty_status, None);
    assert_eq!(parsed.collection_id, None);
}

// ---- ItemOperations Move (conversation move, [MS-ASCMD] §4.25) ----

/// Request shape per [MS-ASCMD] §4.25.1 (ItemOperations code page 20
/// per [MS-ASWBXML] §2.1.2.1.21):
/// ```text
/// ItemOperations (20,0x05) > Move (20,0x16) >
///   DstFldId (20,0x17) = "15",
///   ConversationId (20,0x18) = OPAQUE bytes,
///   Options (20,0x08) > MoveAlways (20,0x19) — empty element
/// ```
/// Exact child sequence: DstFldId first, ConversationId second, Options
/// last. ConversationId serializes as OPAQUE bytes (verbatim — the
/// server treats it as an opaque blob; same convention as the email2
/// ConversationId round-trip tests above).
#[test]
fn conversation_move_request_wire_shape_with_move_always() {
    use tags::item_operations as io;
    let req = ConversationMoveRequest {
        dst_folder_id: "15".to_string(),
        conversation_id: vec![0xDE, 0xAD, 0xBE, 0xEF],
        move_always: true,
    };
    let tree = build_conversation_move_request(&req);
    assert_eq!(
        (tree.page, tree.token),
        (PAGE_ITEM_OPS, io::ITEM_OPERATIONS)
    );
    assert_eq!(tree.children.len(), 1);
    let mv = &tree.children[0];
    assert_eq!((mv.page, mv.token), (PAGE_ITEM_OPS, io::MOVE));
    assert_eq!(mv.children.len(), 3);
    let dst = &mv.children[0];
    assert_eq!((dst.page, dst.token), (PAGE_ITEM_OPS, io::DST_FLD_ID));
    assert_eq!(text_value(dst).expect("dst folder id text"), "15");
    let cid = &mv.children[1];
    assert_eq!((cid.page, cid.token), (PAGE_ITEM_OPS, io::CONVERSATION_ID));
    match &cid.value {
        WbxmlValue::Opaque(b) => assert_eq!(b, &vec![0xDE, 0xAD, 0xBE, 0xEF]),
        other => panic!("ConversationId must serialize as OPAQUE, got {other:?}"),
    }
    let options = &mv.children[2];
    assert_eq!((options.page, options.token), (PAGE_ITEM_OPS, io::OPTIONS));
    assert_eq!(options.children.len(), 1);
    let always = &options.children[0];
    assert_eq!(
        (always.page, always.token),
        (PAGE_ITEM_OPS, io::MOVE_ALWAYS)
    );
    assert!(always.children.is_empty());
    assert!(matches!(always.value, WbxmlValue::Empty));
}

/// When `move_always` is false the whole Options element is omitted —
/// the §4.25.1 shape carries Options only for MoveAlways.
#[test]
fn conversation_move_request_omits_options_without_move_always() {
    use tags::item_operations as io;
    let req = ConversationMoveRequest {
        dst_folder_id: "15".to_string(),
        conversation_id: vec![0x01, 0x02],
        move_always: false,
    };
    let tree = build_conversation_move_request(&req);
    assert_eq!(
        (tree.page, tree.token),
        (PAGE_ITEM_OPS, io::ITEM_OPERATIONS)
    );
    let mv = &tree.children[0];
    assert_eq!((mv.page, mv.token), (PAGE_ITEM_OPS, io::MOVE));
    assert_eq!(mv.children.len(), 2);
    let dst = &mv.children[0];
    assert_eq!((dst.page, dst.token), (PAGE_ITEM_OPS, io::DST_FLD_ID));
    assert_eq!(text_value(dst).expect("dst folder id text"), "15");
    let cid = &mv.children[1];
    assert_eq!((cid.page, cid.token), (PAGE_ITEM_OPS, io::CONVERSATION_ID));
    match &cid.value {
        WbxmlValue::Opaque(b) => assert_eq!(b, &vec![0x01, 0x02]),
        other => panic!("ConversationId must serialize as OPAQUE, got {other:?}"),
    }
}

/// Both request forms survive the WBXML serializer round-trip, opaque
/// ConversationId bytes included.
#[test]
fn conversation_move_request_round_trips() {
    for move_always in [false, true] {
        let req = ConversationMoveRequest {
            dst_folder_id: "15".to_string(),
            conversation_id: vec![0xDE, 0xAD, 0xBE, 0xEF],
            move_always,
        };
        let tree = build_conversation_move_request(&req);
        let back = round_trip(&tree);
        assert_eq!(tree, back);
    }
}

/// Response shape per [MS-ASCMD] §4.25.2:
/// ```text
/// ItemOperations (20,0x05) > Status (20,0x0D) = 1,
///   Response (20,0x0E) > Move (20,0x16) >
///     Status (20,0x0D) = 1, ConversationId (20,0x18) = OPAQUE echo
/// ```
/// Both status levels surface; the ConversationId echo comes back as
/// the same opaque bytes that were sent.
#[test]
fn conversation_move_response_parses_spec_shape() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    io::MOVE,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
                        WbxmlElement::opaque(PAGE_ITEM_OPS, io::CONVERSATION_ID, vec![0xDE, 0xAD]),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_conversation_move_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.move_status, Some(1));
    assert_eq!(parsed.conversation_id, Some(vec![0xDE, 0xAD]));
}

/// Some deployments serialize the ConversationId echo as base64 *text*
/// instead of opaque binary (same dual-form behavior as the email2
/// ConversationId convention). The parser keeps the bytes verbatim in
/// both cases — never base64-decodes.
#[test]
fn conversation_move_response_echo_text_form_is_kept() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    io::MOVE,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
                        WbxmlElement::text(PAGE_ITEM_OPS, io::CONVERSATION_ID, "3q0="),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_conversation_move_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.move_status, Some(1));
    assert_eq!(parsed.conversation_id, Some(b"3q0=".to_vec()));
}

/// Nested-Status rule (mirrors the ItemOperations fetch parser and the
/// Settings family): the Move-level Status overrides the top-level one
/// when both are present (more specific wins), while both remain
/// surfaced — the specific one via `move_status`.
#[test]
fn conversation_move_nested_status_overrides_top_level() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    io::MOVE,
                    vec![WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "4")],
                )],
            ),
        ],
    );
    let parsed = parse_conversation_move_response(&tree).expect("parse");
    assert_eq!(parsed.move_status, Some(4));
    assert_eq!(parsed.status, 4); // more specific wins
    assert_eq!(parsed.conversation_id, None);
}

/// A command-level rejection — top-level Status only (e.g. 143 device
/// not provisioned), no Response element — surfaces on `status`;
/// `move_status` and `conversation_id` stay None.
#[test]
fn conversation_move_response_command_level_error() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "143")],
    );
    let parsed = parse_conversation_move_response(&tree).expect("parse");
    assert_eq!(parsed.status, 143);
    assert_eq!(parsed.move_status, None);
    assert_eq!(parsed.conversation_id, None);
}

/// Absent Status elements default `status` to 1 (success), mirroring
/// the GetItemEstimate/Settings family pattern; `move_status` stays
/// None.
#[test]
fn conversation_move_response_defaults_status_when_absent() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(PAGE_ITEM_OPS, io::ITEM_OPERATIONS, vec![]);
    let parsed = parse_conversation_move_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.move_status, None);
    assert_eq!(parsed.conversation_id, None);
}

/// A missing or empty ConversationId echo parses to `None`, NOT
/// `Some(vec![])` — empty != absent (same rule as the email2
/// ConversationId convention).
#[test]
fn conversation_move_response_empty_echo_is_none() {
    use tags::item_operations as io;
    let tree = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    io::MOVE,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
                        WbxmlElement::empty(PAGE_ITEM_OPS, io::CONVERSATION_ID),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_conversation_move_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.conversation_id, None);
}
