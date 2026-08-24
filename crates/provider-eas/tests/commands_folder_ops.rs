// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::{tests_common::*, *};

#[test]
fn folder_create_round_trips() {
    let req = FolderCreateRequest {
        parent_id: "0".to_string(),
        display_name: "Test Folder".to_string(),
        class: "Email".to_string(),
    };
    let tree = build_folder_create_request(&req, "7");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
    assert_eq!(
        tree.children.first().map(|c| c.token),
        Some(FH_SYNC_KEY),
        "FolderCreate must carry <SyncKey> FIRST per MS-ASCMD"
    );
    if let Some(WbxmlValue::Text(t)) = tree.children.first().map(|c| &c.value) {
        assert_eq!(t, "7");
    } else {
        panic!("expected text SyncKey value");
    }
}

#[test]
fn folder_update_round_trips() {
    let req = FolderUpdateRequest {
        server_id: "fid-1".to_string(),
        parent_id: Some("0".to_string()),
        display_name: Some("Renamed".to_string()),
    };
    let tree = build_folder_update_request(&req, "7");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
    assert_eq!(
        tree.children.first().map(|c| c.token),
        Some(FH_SYNC_KEY),
        "FolderUpdate must carry <SyncKey> FIRST per MS-ASCMD"
    );
    if let Some(WbxmlValue::Text(t)) = tree.children.first().map(|c| &c.value) {
        assert_eq!(t, "7");
    } else {
        panic!("expected text SyncKey value");
    }
}

/// [MS-ASCMD] §6.16 FolderUpdate request schema: ParentId is REQUIRED
/// (1...1) even for a pure rename — it is "the parent folder of the
/// folder to be renamed" (§2.2.3.129.3). Omitting it is a schema
/// violation the server rejects with status 10 (live evidence:
/// 2026-08-02 raw bisect — rename without ParentId → status 10, same
/// request with ParentId "0" → status 1). When the caller does not know
/// the parent, the builder emits "0" (mailbox root).
#[test]
fn folder_update_request_always_emits_parent_id() {
    // Rename without an explicit parent → ParentId "0" (root).
    let req = FolderUpdateRequest {
        server_id: "57".to_string(),
        parent_id: None,
        display_name: Some("Renamed".to_string()),
    };
    let tree = build_folder_update_request(&req, "2");
    let tokens: Vec<u8> = tree.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![FH_SYNC_KEY, FH_SERVER_ID, FH_PARENT_ID, FH_DISPLAY_NAME],
        "schema order: SyncKey, ServerId, ParentId, DisplayName"
    );
    let parent = tree
        .children
        .iter()
        .find(|c| c.token == FH_PARENT_ID)
        .expect("ParentId must always be emitted");
    assert_eq!(text_value(parent).unwrap(), "0");

    // Explicit parent is echoed verbatim.
    let req = FolderUpdateRequest {
        server_id: "57".to_string(),
        parent_id: Some("13".to_string()),
        display_name: Some("Renamed".to_string()),
    };
    let tree = build_folder_update_request(&req, "2");
    let parent = tree
        .children
        .iter()
        .find(|c| c.token == FH_PARENT_ID)
        .expect("ParentId");
    assert_eq!(text_value(parent).unwrap(), "13");
}

#[test]
fn folder_delete_round_trips() {
    let req = FolderDeleteRequest {
        server_id: "fid-1".to_string(),
    };
    let tree = build_folder_delete_request(&req, "7");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
    assert_eq!(
        tree.children.first().map(|c| c.token),
        Some(FH_SYNC_KEY),
        "FolderDelete must carry <SyncKey> FIRST per MS-ASCMD"
    );
    if let Some(WbxmlValue::Text(t)) = tree.children.first().map(|c| &c.value) {
        assert_eq!(t, "7");
    } else {
        panic!("expected text SyncKey value");
    }
}

#[test]
fn folder_op_response_status_only() {
    let response = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_UPDATE,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1")],
    );
    let (status, id) = parse_folder_op_response(&response).expect("parse");
    assert_eq!(status, 1);
    assert!(id.is_none());
}

#[test]
fn folder_op_response_with_server_id() {
    let response = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_CREATE,
        vec![
            WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
            WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, "new-fid"),
        ],
    );
    let (status, id) = parse_folder_op_response(&response).expect("parse");
    assert_eq!(status, 1);
    assert_eq!(id.as_deref(), Some("new-fid"));
}

#[test]
fn folder_type_mapping() {
    assert_eq!(folder_type_to_class("1"), "Email"); // generic
    assert_eq!(folder_type_to_class("2"), "Email"); // inbox
    assert_eq!(folder_type_to_class("3"), "Email"); // drafts
    assert_eq!(folder_type_to_class("4"), "Email"); // deleted
    assert_eq!(folder_type_to_class("5"), "Email"); // sent
    assert_eq!(folder_type_to_class("6"), "Email"); // outbox
    assert_eq!(folder_type_to_class("7"), "Tasks");
    assert_eq!(folder_type_to_class("8"), "Calendar");
    assert_eq!(folder_type_to_class("9"), "Contacts");
    assert_eq!(folder_type_to_class("10"), "Notes");
    assert_eq!(folder_type_to_class("11"), "Notes");
    assert_eq!(folder_type_to_class("12"), "Email"); // junk
    assert_eq!(folder_type_to_class("99"), "Email"); // unknown defaults to Email
}

/// FolderCreate Type values per [MS-ASCMD] 2.2.3.186.2: only 1 and 12–17
/// are valid in a CREATE — 2–11 and 19 are reserved for default folders
/// and are rejected as malformed (status 10). Regression guard: the old
/// mapping returned the DEFAULT-folder codes (Email→2, Calendar→8, …),
/// which Exchange rejects with FolderCreate status 10 (live evidence:
/// eas_folder_debug bisect 2026-08-02 — Type "2" → status 10,
/// Type "12" → status 1 + ServerId).
#[test]
fn class_to_type_mapping() {
    assert_eq!(class_to_folder_type("Email"), "12"); // user-created mail folder
    assert_eq!(class_to_folder_type("Calendar"), "13"); // user-created Calendar
    assert_eq!(class_to_folder_type("Contacts"), "14"); // user-created Contacts
    assert_eq!(class_to_folder_type("Tasks"), "15"); // user-created Tasks
    assert_eq!(class_to_folder_type("Journal"), "16"); // user-created Journal
    assert_eq!(class_to_folder_type("Notes"), "17"); // user-created Notes
    assert_eq!(class_to_folder_type("Unknown"), "1"); // user-created generic
}

/// The FolderCreate request must carry the user-created Type value (12
/// for Email), not a reserved default-folder code.
#[test]
fn folder_create_request_emits_user_created_type() {
    let req = FolderCreateRequest {
        parent_id: "0".to_string(),
        display_name: "Probe".to_string(),
        class: "Email".to_string(),
    };
    let tree = build_folder_create_request(&req, "1");
    let type_el = tree
        .children
        .iter()
        .find(|c| c.page == PAGE_FOLDER && c.token == FH_TYPE)
        .expect("FolderCreate must carry a Type element");
    assert_eq!(text_value(type_el).unwrap(), "12");
}

/// Folder-op responses carry the NEW hierarchy SyncKey ([MS-ASCMD]
/// 2.2.3.181.1) alongside Status/ServerId — the client must be able to
/// extract it so the next folder op uses the current key.
#[test]
fn folder_op_response_sync_key_extracted() {
    let response = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_CREATE,
        vec![
            WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
            WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, "2"),
            WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, "52"),
        ],
    );
    assert_eq!(folder_op_response_sync_key(&response).as_deref(), Some("2"));
    // Absent SyncKey → None (malformed/defensive path).
    let response = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_DELETE,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1")],
    );
    assert_eq!(folder_op_response_sync_key(&response), None);
}

// ---- Phase 3a Task 1: typed EasItem/EasAttachment + SyncResult.status ----

/// `SyncResult::default()` must surface `status = 1` (success) per
/// [MS-ASSYNC] 2.2.3.23. The engine reads this to decide whether to
/// persist the returned sync_key.
#[test]
fn move_items_request_builds_one_move_per_tuple() {
    let moves = vec![
        ("5:12".to_string(), "5".to_string(), "4".to_string()),
        ("5:13".to_string(), "5".to_string(), "4".to_string()),
    ];
    let tree = build_move_items_request(&moves);
    assert_eq!((tree.page, tree.token), (5, 0x05)); // MoveItems
    assert_eq!(tree.children.len(), 2, "one Move child per tuple");
    for (i, m) in tree.children.iter().enumerate() {
        assert_eq!((m.page, m.token), (5, 0x06), "Move token"); // Move
        assert_eq!(m.children.len(), 3);
        assert_eq!((m.children[0].page, m.children[0].token), (5, 0x07)); // SrcMsgId
        assert_eq!((m.children[1].page, m.children[1].token), (5, 0x08)); // SrcFldId
        assert_eq!((m.children[2].page, m.children[2].token), (5, 0x09)); // DstFldId
        assert_eq!(text_value(&m.children[0]).unwrap(), moves[i].0);
        assert_eq!(text_value(&m.children[1]).unwrap(), moves[i].1);
        assert_eq!(text_value(&m.children[2]).unwrap(), moves[i].2);
    }
}

/// An empty move set still builds a valid (childless) MoveItems root; the
/// caller short-circuits before sending so this only guards the builder.
#[test]
fn move_items_request_empty_moves_builds_empty_root() {
    let tree = build_move_items_request(&[]);
    assert_eq!((tree.page, tree.token), (5, 0x05));
    assert!(tree.children.is_empty());
}

#[test]
fn move_items_request_round_trips() {
    let moves = vec![("7:1".to_string(), "7".to_string(), "3".to_string())];
    let tree = build_move_items_request(&moves);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Mixed per-Move statuses parse in Response order: each Response carries
/// its own Status; DstMsgId is present only on success.
#[test]
fn move_items_response_parses_mixed_per_move_statuses() {
    let response = WbxmlElement::container(
        5,
        0x05, // MoveItems
        vec![
            WbxmlElement::container(
                5,
                0x0A, // Response
                vec![
                    WbxmlElement::text(5, 0x07, "5:12"), // SrcMsgId echo
                    WbxmlElement::text(5, 0x0B, "1"),    // Status
                    WbxmlElement::text(5, 0x0C, "4:88"), // DstMsgId
                ],
            ),
            WbxmlElement::container(
                5,
                0x0A, // Response
                vec![
                    WbxmlElement::text(5, 0x07, "5:13"), // SrcMsgId echo
                    WbxmlElement::text(5, 0x0B, "150"),  // Status: item not found
                ],
            ),
        ],
    );
    let parsed = parse_move_items_response(&response).expect("parse");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0], (1, Some("4:88".to_string())));
    assert_eq!(parsed[1], (150, None));
}

/// A non-MoveItems root is a parse error, not a silent success.
#[test]
fn move_items_response_rejects_wrong_root() {
    let response = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    assert!(parse_move_items_response(&response).is_err());
}

/// The client gate: the FIRST per-Move result `move_status_succeeded`
/// rejects wins; all-success (or empty) yields None.
#[test]
fn first_failing_move_status_picks_first_non_success() {
    let ok = vec![
        (1u32, Some("4:1".to_string())),
        (1, Some("4:2".to_string())),
    ];
    assert_eq!(first_failing_move_status(&ok), None);
    assert_eq!(first_failing_move_status(&[]), None);

    let mixed = vec![(1u32, None), (153, None), (156, None)];
    assert_eq!(first_failing_move_status(&mixed), Some(153));
}

// ---- F10-2: MoveItems Status 3 IS the success code ----
//
// [MS-ASCMD] 2.2.3.177.10: MoveItems is the one command whose SUCCESS
// status is **3**, not 1 — 1 means "invalid source collection/item ID".
// Android's MoveItemsParser maps 3 (and 4/6) to success the same way.
// Live evidence 2026-08-02 (Exchange 15.2): per-Move Status 3 arrives
// WITH a valid DstMsgId and the move is performed (IMAP-verified).

/// Spec success shapes: status 1 (legacy tolerance) and status 3 WITH a
/// non-empty DstMsgId succeed; everything else fails — including a bare
/// status 3 without a DstMsgId (we cannot hand the caller the moved
/// item's new id, so surfacing it is safer than a silent "success").
#[test]
fn move_status_succeeded_spec_success_code_3() {
    assert!(move_status_succeeded(1, None));
    assert!(move_status_succeeded(1, Some("4:1")));
    assert!(move_status_succeeded(3, Some("4:88")));
    assert!(
        !move_status_succeeded(3, None),
        "bare 3 without DstMsgId fails"
    );
    assert!(!move_status_succeeded(3, Some("")), "empty DstMsgId fails");
    assert!(!move_status_succeeded(2, Some("4:1")));
    assert!(!move_status_succeeded(4, None));
    assert!(!move_status_succeeded(7, None));
    assert!(!move_status_succeeded(150, None));
}

/// The batch gate tolerates the Exchange 15.2 shape: an all-3-with-DstMsgId
/// batch is full success; a bare 3 still surfaces.
#[test]
fn first_failing_move_status_tolerates_status_3_with_dst_msg_id() {
    let exchange_15_2 = vec![
        (3u32, Some("4:88".to_string())),
        (3u32, Some("4:89".to_string())),
    ];
    assert_eq!(first_failing_move_status(&exchange_15_2), None);

    let bare_three = vec![(3u32, Some("4:88".to_string())), (3u32, None)];
    assert_eq!(first_failing_move_status(&bare_three), Some(3));

    // Other non-success statuses still fail, first-failing wins.
    let mixed = vec![(3u32, Some("4:88".to_string())), (2u32, None), (1u32, None)];
    assert_eq!(first_failing_move_status(&mixed), Some(2));
}

/// The MoveItems status table ([MS-ASCMD] 2.2.3.177.10) — note the
/// inversion versus every other command (3 = success, 1 = error).
#[test]
fn move_items_status_message_maps_spec_table() {
    assert_eq!(
        move_items_status_message(1),
        "invalid source collection ID or source item ID"
    );
    assert_eq!(
        move_items_status_message(2),
        "invalid destination collection ID"
    );
    assert_eq!(move_items_status_message(3), "success");
    assert_eq!(
        move_items_status_message(4),
        "source and destination collections are the same"
    );
    assert_eq!(
        move_items_status_message(7),
        "source or destination item locked (transient — retry)"
    );
    // Out-of-table codes fall back to the common table, never "unknown"
    // for a listed common code.
    assert_eq!(move_items_status_message(150), "item not found");
    assert_eq!(move_items_status_message(999), "unknown status code");
}
