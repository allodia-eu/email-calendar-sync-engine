// SPDX-License-Identifier: MPL-2.0
// ItemOperations Move — conversation move ([MS-ASCMD] §4.25).

use crate::commands::{
    ConversationMoveRequest, ConversationMoveResult, PAGE_ITEM_OPS, WbxmlElement, WbxmlError,
    WbxmlValue, expect_tag, tags, text_value,
};

// ============================================================================
// ItemOperations → Move (conversation move)
// ============================================================================

/// Build an ItemOperations → Move request ([MS-ASCMD] §4.25.1;
/// ItemOperations code page 20 per [MS-ASWBXML] §2.1.2.1.21). This is a
/// SEPARATE builder from `build_item_operations_request` — the fetch
/// builder's shape is Fetch-specific and must not be overloaded. Note the
/// Move token here is the ItemOperations-namespace (page 20) 0x16 — NOT
/// the MoveItems-page (5) Move token.
///
/// Wire shape:
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Move>                              <!-- page 20, 0x16 -->
///     <DstFldId>15</DstFldId>           <!-- page 20, 0x17 -->
///     <ConversationId>...</>            <!-- page 20, 0x18 — OPAQUE bytes -->
///     [<Options>                        <!-- page 20, 0x08 -->
///       <MoveAlways/>                   <!-- page 20, 0x19 — empty element -->
///     </Options>]
///   </Move>
/// </ItemOperations>
/// ```
/// ConversationId is serialized as OPAQUE binary, carried verbatim (never
/// base64-decoded or re-encoded) — the same convention as the email2
/// ConversationId parse path. The Options element (and its MoveAlways
/// child) is emitted ONLY when `req.move_always` is true.
///
/// MOVE_ALWAYS moves all FUTURE messages of the conversation to the
/// destination folder too — a persistent server-side behavior. Callers
/// must surface this to the user before setting it.
pub fn build_conversation_move_request(req: &ConversationMoveRequest) -> WbxmlElement {
    use tags::item_operations as io;

    let mut move_children = vec![
        WbxmlElement::text(PAGE_ITEM_OPS, io::DST_FLD_ID, req.dst_folder_id.clone()),
        WbxmlElement::opaque(
            PAGE_ITEM_OPS,
            io::CONVERSATION_ID,
            req.conversation_id.clone(),
        ),
    ];
    if req.move_always {
        move_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            vec![WbxmlElement::empty(PAGE_ITEM_OPS, io::MOVE_ALWAYS)],
        ));
    }

    WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::MOVE,
            move_children,
        )],
    )
}

/// Parse an ItemOperations → Move response ([MS-ASCMD] §4.25.2):
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Status>1</Status>                  <!-- page 20, 0x0D — command level -->
///   <Response>                          <!-- page 20, 0x0E -->
///     <Move>                            <!-- page 20, 0x16 -->
///       <Status>1</Status>              <!-- page 20, 0x0D — element level -->
///       <ConversationId>...</>          <!-- page 20, 0x18 — echo -->
///     </Move>
///   </Response>
/// </ItemOperations>
/// ```
/// Nested-Status rule mirrors the ItemOperations fetch parser and the
/// Settings family: the top-level Status is read first (command-level
/// rejection, e.g. 143 device not provisioned — then no Response element
/// is present at all), and the Move-level Status overrides it when present
/// (more specific wins). Both stay surfaced: the specific one on
/// `move_status`. A missing Status defaults to 1 (success), mirroring
/// GetItemEstimate. Malformed Status values are warn-logged and defaulted
/// — never swallowed.
///
/// The ConversationId echo is opaque binary on the wire, but some
/// deployments serialize it as base64 *text* — handle both and keep the
/// bytes verbatim (never decoded), the same convention as the email2
/// ConversationId path. A missing or empty echo maps to `None` (not
/// `Some(vec![])`), since empty != absent.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_conversation_move_response(
    root: &WbxmlElement,
) -> Result<ConversationMoveResult, WbxmlError> {
    use tags::item_operations as io;

    expect_tag(root, PAGE_ITEM_OPS, io::ITEM_OPERATIONS)?;
    let mut result = ConversationMoveResult {
        status: 1, // success default when Status elements are absent
        ..ConversationMoveResult::default()
    };
    // Top-level Status first; a Move-level Status below overrides it (same
    // ordering as parse_item_operations_response).
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!(
                    "ItemOperations Move: malformed top-level Status \"{raw}\"; defaulting to 1"
                );
                1
            };
        }
    }
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_ITEM_OPS && resp_child.token == io::MOVE {
                    for move_child in &resp_child.children {
                        match (move_child.page, move_child.token) {
                            (PAGE_ITEM_OPS, io::STATUS) => {
                                let raw = text_value(move_child).unwrap_or_default();
                                let n: u32 = if let Ok(n) = raw.parse() {
                                    n
                                } else {
                                    log::warn!(
                                        "ItemOperations Move: malformed Move Status \"{raw}\"; defaulting to 1"
                                    );
                                    1
                                };
                                result.move_status = Some(n);
                                result.status = n; // more specific wins
                            }
                            (PAGE_ITEM_OPS, io::CONVERSATION_ID) => {
                                result.conversation_id = match &move_child.value {
                                    WbxmlValue::Opaque(b) if !b.is_empty() => Some(b.clone()),
                                    WbxmlValue::Text(s) if !s.is_empty() => {
                                        Some(s.as_bytes().to_vec())
                                    }
                                    _ => None,
                                };
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}
