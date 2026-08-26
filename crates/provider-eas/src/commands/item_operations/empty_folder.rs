// SPDX-License-Identifier: MPL-2.0
// ItemOperations EmptyFolderContents ([MS-ASCMD] §4.14.4).

use crate::commands::{
    AS_COLLECTION_ID, EmptyFolderContentsRequest, EmptyFolderContentsResult, PAGE_AIRSYNC,
    PAGE_ITEM_OPS, WbxmlElement, WbxmlError, expect_tag, tags, text_value,
};

// ============================================================================
// ItemOperations → EmptyFolderContents
// ============================================================================

/// Build an ItemOperations → EmptyFolderContents request ([MS-ASCMD]
/// §4.14.4.1; ItemOperations code page 20 per [MS-ASWBXML] §2.1.2.1.21).
/// This is a SEPARATE builder from `build_item_operations_request` — the
/// fetch builder's shape is Fetch-specific and must not be overloaded.
///
/// Wire shape:
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <EmptyFolderContents>               <!-- page 20, 0x12 -->
///     <airsync:CollectionId>15</>       <!-- page 0, 0x12 — AirSync page -->
///     [<Options>                        <!-- page 20, 0x08 -->
///       <DeleteSubFolders/>             <!-- page 20, 0x13 — empty element -->
///     </Options>]
///   </EmptyFolderContents>
/// </ItemOperations>
/// ```
/// The Options element (and its DeleteSubFolders child) is emitted ONLY
/// when `req.delete_sub_folders` is true — the server default keeps
/// subfolders, and the §4.14.4.1 example carries no Options at all.
///
/// DESTRUCTIVE: this deletes every item in the folder server-side (and,
/// with `delete_sub_folders`, the subfolders too). Callers must confirm
/// with the user before invoking.
pub fn build_empty_folder_contents_request(req: &EmptyFolderContentsRequest) -> WbxmlElement {
    use tags::item_operations as io;

    let mut efc_children = vec![WbxmlElement::text(
        PAGE_AIRSYNC,
        AS_COLLECTION_ID,
        req.collection_id.clone(),
    )];
    if req.delete_sub_folders {
        efc_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            vec![WbxmlElement::empty(PAGE_ITEM_OPS, io::DELETE_SUB_FOLDERS)],
        ));
    }

    WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::EMPTY_FOLDER_CONTENTS,
            efc_children,
        )],
    )
}

/// Parse an ItemOperations → EmptyFolderContents response ([MS-ASCMD]
/// §4.14.4.2):
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Status>1</Status>                  <!-- page 20, 0x0D — command level -->
///   <Response>                          <!-- page 20, 0x0E -->
///     <EmptyFolderContents>             <!-- page 20, 0x12 -->
///       <Status>1</Status>              <!-- page 20, 0x0D — element level -->
///       <airsync:CollectionId>15</>     <!-- page 0, 0x12 — echo -->
///     </EmptyFolderContents>
///   </Response>
/// </ItemOperations>
/// ```
/// Nested-Status rule mirrors the ItemOperations fetch parser and the
/// Settings family: the top-level Status is read first (command-level
/// rejection, e.g. 143 device not provisioned — then no Response element
/// is present at all), and the EmptyFolderContents-level Status overrides
/// it when present (more specific wins). Both stay surfaced: the specific
/// one on `empty_status`. A missing Status defaults to 1 (success),
/// mirroring GetItemEstimate. Malformed Status values are warn-logged and
/// defaulted — never swallowed.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_empty_folder_contents_response(
    root: &WbxmlElement,
) -> Result<EmptyFolderContentsResult, WbxmlError> {
    use tags::item_operations as io;

    expect_tag(root, PAGE_ITEM_OPS, io::ITEM_OPERATIONS)?;
    let mut result = EmptyFolderContentsResult {
        status: 1, // success default when Status elements are absent
        ..EmptyFolderContentsResult::default()
    };
    // Top-level Status first; an EmptyFolderContents-level Status below
    // overrides it (same ordering as parse_item_operations_response).
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!(
                    "ItemOperations EmptyFolderContents: malformed top-level Status \"{raw}\"; defaulting to 1"
                );
                1
            };
        }
    }
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_ITEM_OPS && resp_child.token == io::EMPTY_FOLDER_CONTENTS
                {
                    for efc_child in &resp_child.children {
                        match (efc_child.page, efc_child.token) {
                            (PAGE_ITEM_OPS, io::STATUS) => {
                                let raw = text_value(efc_child).unwrap_or_default();
                                let n: u32 = if let Ok(n) = raw.parse() {
                                    n
                                } else {
                                    log::warn!(
                                        "ItemOperations EmptyFolderContents: malformed EmptyFolderContents Status \"{raw}\"; defaulting to 1"
                                    );
                                    1
                                };
                                result.empty_status = Some(n);
                                result.status = n; // more specific wins
                            }
                            (PAGE_AIRSYNC, AS_COLLECTION_ID) => {
                                result.collection_id = match text_value(efc_child) {
                                    Ok(s) => Some(s),
                                    Err(e) => {
                                        // Undecodable CollectionId is malformed
                                        // server data; drop the echo but never
                                        // silently.
                                        log::warn!(
                                            "ItemOperations EmptyFolderContents: skipping undecodable CollectionId echo: {e}"
                                        );
                                        None
                                    }
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
