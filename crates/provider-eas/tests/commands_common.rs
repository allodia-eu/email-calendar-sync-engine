// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::*;

#[test]
fn common_status_message_covers_reference_table() {
    assert_eq!(common_status_message(101), Some("invalid content"));
    assert_eq!(
        common_status_message(108),
        Some("device ID missing or invalid format")
    );
    assert_eq!(
        common_status_message(109),
        Some("device type missing or invalid")
    );
    assert_eq!(common_status_message(142), Some("device not provisioned"));
    assert_eq!(common_status_message(147), Some("unexpected item class"));
    assert_eq!(common_status_message(999), None);
}

#[test]
fn top_level_status_reads_each_command_page() {
    let cases: [(u8, u8, &str, u32); 5] = [
        (PAGE_AIRSYNC, AS_STATUS, "142", 142),
        (PAGE_FOLDER, FH_STATUS, "108", 108),
        (PAGE_PING, PING_STATUS, "1", 1),
        (PAGE_ITEM_OPS, tags::item_operations::STATUS, "143", 143),
        (PAGE_COMPOSE, compose::STATUS, "144", 144),
    ];
    for (page, status_token, text, want) in cases {
        let root = WbxmlElement::container(
            page,
            0x05, // any root token — only children are inspected
            vec![WbxmlElement::text(page, status_token, text)],
        );
        assert_eq!(top_level_status(&root), Some(want), "page {page}");
    }
}

#[test]
fn top_level_status_none_when_absent_or_unknown_page() {
    // No Status child:
    let root = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    assert_eq!(top_level_status(&root), None);
    // Non-numeric Status:
    let root = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "abc")],
    );
    assert_eq!(top_level_status(&root), None);
    // Unknown page (e.g. GIE 0x06 has no table entry):
    let root = WbxmlElement::container(0x06, 0x05, vec![WbxmlElement::text(0x06, 0x03, "1")]);
    assert_eq!(top_level_status(&root), None);
}

// ---- Settings (DeviceInformation) + common status 148–177 ----

#[test]
fn common_status_message_covers_148_to_177() {
    assert_eq!(common_status_message(148), Some("remote server has no SSL"));
    assert_eq!(
        common_status_message(165),
        Some("device information required — send Settings DeviceInformation first")
    );
    assert_eq!(common_status_message(168), Some("IRM feature disabled"));
    assert_eq!(common_status_message(177), Some("maximum devices reached"));
    // 157–159 are unassigned in MS-ASCMD:
    assert_eq!(common_status_message(157), None);
}
