// SPDX-License-Identifier: MPL-2.0
//! build_sync_request Options shaping: BodyPreference and truncation.

use super::*;

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
