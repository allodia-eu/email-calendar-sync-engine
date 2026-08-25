// SPDX-License-Identifier: MPL-2.0
//! FolderSync-command tests: hierarchy request/response round trips.
use provider_eas::commands::{tests_common::*, *};

#[test]
fn folder_sync_request_minimal() {
    let tree = build_folder_sync_request("0");
    assert_eq!(tree.page, PAGE_FOLDER);
    assert_eq!(tree.token, FH_FOLDER_SYNC);
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].token, FH_SYNC_KEY);
    if let WbxmlValue::Text(t) = &tree.children[0].value {
        assert_eq!(t, "0");
    } else {
        panic!("expected text value");
    }
}

#[test]
fn folder_sync_request_round_trips() {
    let tree = build_folder_sync_request("abc123");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

#[test]
fn folder_sync_response_parses() {
    // Build a synthetic FolderSync response with one add and one delete
    let response = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![
            WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, "new-key-456"),
            WbxmlElement::container(
                PAGE_FOLDER,
                FH_CHANGES,
                vec![
                    WbxmlElement::container(
                        PAGE_FOLDER,
                        FH_ADD,
                        vec![
                            WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, "fid-1"),
                            WbxmlElement::text(PAGE_FOLDER, FH_PARENT_ID, "0"),
                            WbxmlElement::text(PAGE_FOLDER, FH_DISPLAY_NAME, "Inbox"),
                            WbxmlElement::text(PAGE_FOLDER, FH_TYPE, "2"),
                        ],
                    ),
                    WbxmlElement::container(
                        PAGE_FOLDER,
                        FH_DELETE,
                        vec![WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, "fid-old")],
                    ),
                ],
            ),
        ],
    );
    let parsed = parse_folder_sync_response(&response).expect("parse");
    assert_eq!(parsed.sync_key, "new-key-456");
    assert_eq!(parsed.changes.len(), 1);
    assert_eq!(parsed.changes[0].server_id, "fid-1");
    assert_eq!(parsed.changes[0].display_name, "Inbox");
    assert_eq!(parsed.changes[0].class, "Email"); // type 2 = Inbox → Email
    assert_eq!(parsed.changes[0].folder_type, Some(2)); // raw Type byte surfaced
    assert_eq!(parsed.deletions, vec!["fid-old".to_string()]);
}

#[test]
fn folder_sync_non_success_status_is_data_not_error() {
    let tree = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "108")],
    );
    let result = parse_folder_sync_response(&tree).expect("status 108 must parse, not error");
    assert_eq!(result.status, 108);
}

#[test]
fn folder_sync_missing_status_defaults_to_success() {
    let tree = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, "1")],
    );
    let result = parse_folder_sync_response(&tree).expect("parse");
    assert_eq!(result.status, 1);
}

#[test]
fn folder_sync_status_message_falls_back_to_common_table() {
    assert_eq!(folder_sync_status_message(1), "success");
    assert_eq!(
        folder_sync_status_message(6),
        "synchronization state is not current"
    );
    // MS-ASCMD common codes, reachable via fallback:
    assert_eq!(
        folder_sync_status_message(108),
        "device ID missing or invalid format"
    );
    // Deliberate correction: 126 was previously (wrongly) "provision
    // required"; MS-ASCMD defines it as userDisabledForSync.
    assert_eq!(folder_sync_status_message(126), "user disabled for sync");
    assert_eq!(folder_sync_status_message(999), "unknown status code");
}

// ---- top_level_status (generic in-body provision-retry support) ----
//
// NOTE on constants: the ItemOperations and ComposeMail rows use
// `tags::item_operations::STATUS` (0x0D) and `compose::STATUS` (0x12) —
// the spec-correct values. The old file-local `IO_STATUS` constant (0x0A,
// actually `Total` per code_pages.rs ITEMS_TOKENS) and the wrong `CM_*`
// aliases were deleted for being off-spec; `top_level_status` matches on
// the tags.rs values so real server responses are found.
