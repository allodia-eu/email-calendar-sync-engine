// SPDX-License-Identifier: MPL-2.0
//! ItemOperations EmptyFolderContents ([MS-ASCMD] §4.14.4).

use super::*;

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
