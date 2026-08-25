// SPDX-License-Identifier: MPL-2.0
//! build_sync_request Collection shape: Class omission, GetChanges, DeletesAsMoves.

use super::*;

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
