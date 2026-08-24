// SPDX-License-Identifier: MPL-2.0
use super::*;

// ============================================================================
// Folder create / update / delete
// ============================================================================

pub fn build_folder_create_request(req: &FolderCreateRequest, sync_key: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_CREATE,
        vec![
            // MS-ASCMD orders <SyncKey> first in all three folder ops.
            WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, sync_key),
            WbxmlElement::text(PAGE_FOLDER, FH_PARENT_ID, req.parent_id.clone()),
            WbxmlElement::text(PAGE_FOLDER, FH_DISPLAY_NAME, req.display_name.clone()),
            WbxmlElement::text(PAGE_FOLDER, FH_TYPE, class_to_folder_type(&req.class)),
        ],
    )
}

pub fn build_folder_update_request(req: &FolderUpdateRequest, sync_key: &str) -> WbxmlElement {
    // MS-ASCMD §6.16 schema order: SyncKey, ServerId, ParentId, DisplayName.
    // ParentId is REQUIRED (1...1) even for a pure rename — §2.2.3.129.3
    // defines it as "the parent folder of the folder to be renamed", and
    // Exchange rejects a ParentId-less update with status 10 (live evidence
    // 2026-08-02). Callers that don't know the current parent get "0"
    // (mailbox root) — correct for root-level folders, and a rename of a
    // nested folder should always pass the real parent explicitly.
    let parent_id = req.parent_id.clone().unwrap_or_else(|| "0".to_string());
    let mut children = vec![
        WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, sync_key),
        WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, req.server_id.clone()),
        WbxmlElement::text(PAGE_FOLDER, FH_PARENT_ID, parent_id),
    ];
    if let Some(name) = &req.display_name {
        children.push(WbxmlElement::text(
            PAGE_FOLDER,
            FH_DISPLAY_NAME,
            name.clone(),
        ));
    }
    WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_UPDATE, children)
}

pub fn build_folder_delete_request(req: &FolderDeleteRequest, sync_key: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_DELETE,
        vec![
            // MS-ASCMD orders <SyncKey> first in all three folder ops.
            WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, sync_key),
            WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, req.server_id.clone()),
        ],
    )
}

/// Parse a FolderCreate/Update/Delete response. All three return a Status code;
/// Create also returns a new ServerId.
pub fn parse_folder_op_response(root: &WbxmlElement) -> Result<(u32, Option<String>), WbxmlError> {
    let mut status: u32 = 1;
    let mut new_server_id: Option<String> = None;
    for child in &root.children {
        if child.page == PAGE_FOLDER && child.token == FH_STATUS {
            let s = text_value(child).unwrap_or("1".to_string());
            status = s.parse().unwrap_or(1);
        }
        if child.page == PAGE_FOLDER && child.token == FH_SERVER_ID {
            new_server_id = Some(text_value(child)?);
        }
    }
    Ok((status, new_server_id))
}

/// Extract the NEW hierarchy SyncKey from a FolderCreate/Update/Delete
/// response ([MS-ASCMD] 2.2.3.181.1 — every successful folder op returns it).
/// The client must adopt this key or the next folder op is sent with a stale
/// SyncKey. `None` when the element is absent (error responses may omit it).
pub fn folder_op_response_sync_key(root: &WbxmlElement) -> Option<String> {
    root.children
        .iter()
        .find(|c| c.page == PAGE_FOLDER && c.token == FH_SYNC_KEY)
        .and_then(text_value_opt)
}

/// Map an item class to the FolderCreate **Type** value per [MS-ASCMD]
/// 2.2.3.186.2. Only 1 and 12–17 are valid in a CREATE — 2–11 and 19 are
/// reserved for default folders (Inbox=2, Calendar=8, …) and are rejected
/// with FolderCreate status 10 ("request is incorrectly formatted"). Live
/// evidence: eas_folder_debug bisect 2026-08-02 — Type "2" → status 10,
/// Type "12" → status 1 + ServerId 52 against Exchange 2019.
///
/// NOTE the asymmetry with `folder_type_to_class`: that one parses the
/// FolderSync response table (where 2/8/9/7 ARE the valid values for the
/// default folders), while this one builds the FolderCreate request table.
pub fn class_to_folder_type(class: &str) -> String {
    match class {
        "Email" => "12".to_string(), // user-created mail folder
        "Calendar" => "13".to_string(),
        "Contacts" => "14".to_string(),
        "Tasks" => "15".to_string(),
        "Journal" => "16".to_string(),
        "Notes" => "17".to_string(),
        _ => "1".to_string(), // user-created generic
    }
}

// ============================================================================
// MoveItems (code page 5)
// ============================================================================
//
// Token table verified against [MS-ASWBXML] §2.1.2.1.6 (page 5):
//   MoveItems=0x05, Move=0x06, SrcMsgId=0x07, SrcFldId=0x08, DstFldId=0x09,
//   Response=0x0A, Status=0x0B, DstMsgId=0x0C.

const PAGE_MOVE: u8 = 5;
const MV_MOVE_ITEMS: u8 = 0x05;
const MV_MOVE: u8 = 0x06;
const MV_SRC_MSG_ID: u8 = 0x07;
const MV_SRC_FLD_ID: u8 = 0x08;
const MV_DST_FLD_ID: u8 = 0x09;
const MV_RESPONSE: u8 = 0x0A;
const MV_STATUS: u8 = 0x0B;
const MV_DST_MSG_ID: u8 = 0x0C;

/// Build a MoveItems request — one `Move` child per
/// `(src_msg_id, src_fld_id, dst_fld_id)` tuple, batched into a single
/// request per [MS-ASCMD] §2.2.1.12.
///
/// WBXML shape:
/// ```xml
/// <MoveItems>                      <!-- page 5, 0x05 -->
///   <Move>                         <!-- page 5, 0x06 -->
///     <SrcMsgId>{src_msg_id}</>    <!-- page 5, 0x07 -->
///     <SrcFldId>{src_fld_id}</>    <!-- page 5, 0x08 -->
///     <DstFldId>{dst_fld_id}</>    <!-- page 5, 0x09 -->
///   </Move>
///   …
/// </MoveItems>
/// ```
pub fn build_move_items_request(moves: &[(String, String, String)]) -> WbxmlElement {
    let move_elements = moves
        .iter()
        .map(|(src_msg_id, src_fld_id, dst_fld_id)| {
            WbxmlElement::container(
                PAGE_MOVE,
                MV_MOVE,
                vec![
                    WbxmlElement::text(PAGE_MOVE, MV_SRC_MSG_ID, src_msg_id.clone()),
                    WbxmlElement::text(PAGE_MOVE, MV_SRC_FLD_ID, src_fld_id.clone()),
                    WbxmlElement::text(PAGE_MOVE, MV_DST_FLD_ID, dst_fld_id.clone()),
                ],
            )
        })
        .collect();
    WbxmlElement::container(PAGE_MOVE, MV_MOVE_ITEMS, move_elements)
}

/// Parse a MoveItems response into per-Move `(Status, DstMsgId)` pairs in
/// `Response` order ([MS-ASCMD] §2.2.1.12 response schema):
/// ```xml
/// <MoveItems>              <!-- page 5, 0x05 -->
///   <Response>             <!-- page 5, 0x0A -->
///     <SrcMsgId>..</>      <!-- echo of the request id — ignored -->
///     <Status>1</Status>   <!-- page 5, 0x0B -->
///     <DstMsgId>4:88</>    <!-- page 5, 0x0C; success only -->
///   </Response>
///   …
/// </MoveItems>
/// ```
/// A missing Status defaults to 1 (success), matching the convention of the
/// other parsers in this file; `DstMsgId` is `None` when absent (non-success
/// responses omit it).
pub fn parse_move_items_response(
    root: &WbxmlElement,
) -> Result<Vec<(u32, Option<String>)>, WbxmlError> {
    expect_tag(root, PAGE_MOVE, MV_MOVE_ITEMS)?;
    let mut out = Vec::new();
    for child in &root.children {
        if (child.page, child.token) != (PAGE_MOVE, MV_RESPONSE) {
            continue;
        }
        let mut status: u32 = 1;
        let mut dst_msg_id: Option<String> = None;
        for c in &child.children {
            match (c.page, c.token) {
                (PAGE_MOVE, MV_STATUS) => {
                    let s = text_value(c).unwrap_or_default();
                    status = s.parse().unwrap_or(1);
                }
                (PAGE_MOVE, MV_DST_MSG_ID) => dst_msg_id = text_value_opt(c),
                _ => {} // SrcMsgId echo — ignored
            }
        }
        out.push((status, dst_msg_id));
    }
    Ok(out)
}

/// Per-Move success predicate for MoveItems responses.
///
/// Spec note ([MS-ASCMD] 2.2.3.177.10): MoveItems is the one command whose
/// SUCCESS status is **3**, not 1 — 1 means "invalid source collection/item
/// ID". Android's `MoveItemsParser` maps 3 (and 4/6) to success the same way.
/// Live evidence (F10-2, Exchange 15.2, 2026-08-02): the per-Move response
/// carries Status 3 WITH a valid DstMsgId and the move is performed
/// (IMAP-verified) — the pre-fix gate treated it as a fatal error.
///
/// Accepted as success:
///   * `1` — the generic EAS success code, tolerated in case a server answers with it (none
///     observed; harmless).
///   * `3` with a non-empty DstMsgId — the spec success shape. A bare 3 without a DstMsgId is
///     treated as failure: we cannot hand the caller the moved item's new id, so surfacing it is
///     safer than a silent "success".
pub fn move_status_succeeded(status: u32, dst_msg_id: Option<&str>) -> bool {
    status == 1 || (status == 3 && dst_msg_id.map(|s| !s.is_empty()).unwrap_or(false))
}

/// The per-Move status gate the client applies after parsing: the FIRST
/// result `move_status_succeeded` rejects wins; all-success (or empty)
/// yields `None`. Pure / no I/O so the batch-failure policy is unit-testable
/// without a live server.
pub fn first_failing_move_status(results: &[(u32, Option<String>)]) -> Option<u32> {
    results
        .iter()
        .find(|(s, d)| !move_status_succeeded(*s, d.as_deref()))
        .map(|(s, _)| *s)
}

/// MoveItems per-Move status codes per [MS-ASCMD] 2.2.3.177.10. NOTE the
/// inversion versus every other command: **3 is SUCCESS** here and 1 is an
/// error. Status 6 ("item already exists in destination") is from Android's
/// `MoveItemsParser` — the v20220429 spec table lists only 1/2/3/4/5/7.
/// Out-of-table codes fall back to `common_status_message`.
pub fn move_items_status_message(status: u32) -> &'static str {
    match status {
        1 => "invalid source collection ID or source item ID",
        2 => "invalid destination collection ID",
        3 => "success",
        4 => "source and destination collections are the same",
        5 => "multiple destination folders in one request, or item locked",
        6 => "item already exists in destination",
        7 => "source or destination item locked (transient — retry)",
        _ => common_status_message(status).unwrap_or("unknown status code"),
    }
}
