// SPDX-License-Identifier: MPL-2.0
//! ItemOperations fetch requests: spec pages/tokens, long-id, file-reference, MIME support.

use super::*;

/// Whether any element in the tree is the AirSync-page MIMESupport token.
fn contains_mime_support(el: &WbxmlElement) -> bool {
    if el.page == PAGE_AIRSYNC && el.token == 0x22 {
        return true;
    }
    el.children.iter().any(contains_mime_support)
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
//   code_pages/pages_00_09.rs AIRSYNC_TOKENS).

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
    assert!(
        !contains_mime_support(&tree),
        "MIMESupport must NOT appear when mime=false"
    );
}
