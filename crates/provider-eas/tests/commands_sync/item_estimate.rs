// SPDX-License-Identifier: MPL-2.0
//! GetItemEstimate requests/responses and the Sync request round-trip.

use super::*;

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
