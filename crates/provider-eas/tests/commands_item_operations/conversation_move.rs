// SPDX-License-Identifier: MPL-2.0
//! ItemOperations Move (conversation move, [MS-ASCMD] §4.25).

use super::*;

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
