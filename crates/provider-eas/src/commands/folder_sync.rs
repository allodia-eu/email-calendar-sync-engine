// SPDX-License-Identifier: MPL-2.0
use super::{
    AS_STATUS, EasFolder, FH_ADD, FH_CHANGES, FH_DELETE, FH_DISPLAY_NAME, FH_FOLDER_SYNC,
    FH_PARENT_ID, FH_SERVER_ID, FH_STATUS, FH_SYNC_KEY, FH_TYPE, FH_UPDATE, FolderSyncResult,
    PAGE_AIRSYNC, PAGE_COMPOSE, PAGE_FOLDER, PAGE_ITEM_OPS, PAGE_PING, PING_STATUS, WbxmlElement,
    WbxmlError, WbxmlValue, compose, expect_tag, tags, text_value, text_value_opt,
};

// ============================================================================
// FolderSync
// ============================================================================

/// Build a FolderSync request.
///
/// WBXML shape:
/// ```xml
/// <FolderSync>
///   <SyncKey>{sync_key}</SyncKey>
/// </FolderSync>
/// ```
pub fn build_folder_sync_request(sync_key: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, sync_key)],
    )
}

/// Parse a FolderSync response.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_folder_sync_response(root: &WbxmlElement) -> Result<FolderSyncResult, WbxmlError> {
    expect_tag(root, PAGE_FOLDER, FH_FOLDER_SYNC)?;

    let mut result = FolderSyncResult {
        status: 1, // success default per [MS-ASFolderSync] 2.2.3.1.10
        ..FolderSyncResult::default()
    };
    for child in &root.children {
        match child.token {
            FH_SYNC_KEY if child.page == PAGE_FOLDER => {
                result.sync_key = text_value(child)?;
            }
            FH_CHANGES if child.page == PAGE_FOLDER => {
                parse_folder_changes(child, &mut result)?;
            }
            FH_STATUS if child.page == PAGE_FOLDER => {
                let s = text_value(child).unwrap_or_default();
                result.status = s.parse().unwrap_or(1);
            }
            _ => {}
        }
    }

    // Non-success statuses are data, not parse failures — the client call
    // site surfaces them as EasError::CommandStatus.
    Ok(result)
}

/// Map a FolderSync status code ([MS-ASCMD] §2.2.3.177.6) to its
/// human-readable meaning.
pub fn folder_sync_status_message(status: u32) -> &'static str {
    match status {
        1 => "success",
        3 => "invalid synchronization key",
        4 => "malformed request",
        5 => "synchronization state no longer exists",
        6 => "synchronization state is not current",
        9 => "folder hierarchy out of date",
        12 => "back-end database unavailable",
        _ => common_status_message(status).unwrap_or("unknown status code"),
    }
}

/// MS-ASCMD common status codes (101–177), returnable by any command.
/// `None` for codes outside the table.
pub fn common_status_message(status: u32) -> Option<&'static str> {
    Some(match status {
        101 => "invalid content",
        102 => "invalid WBXML",
        103 => "invalid XML",
        104 => "invalid datetime",
        105 => "invalid combination of IDs",
        106 => "invalid IDs",
        107 => "invalid MIME",
        108 => "device ID missing or invalid format",
        109 => "device type missing or invalid",
        110 => "server error (do not retry)",
        111 => "server error (retry later)",
        112 => "Active Directory access denied",
        113 => "mailbox quota exceeded",
        114 => "mailbox server offline",
        115 => "send quota exceeded",
        116 => "message recipient unresolved",
        117 => "message reply not allowed",
        118 => "message previously sent",
        119 => "message has no recipient",
        120 => "mail submission failed",
        121 => "message reply failed",
        122 => "attachment too large",
        123 => "user has no mailbox",
        124 => "user cannot be anonymous",
        125 => "user principal not found",
        126 => "user disabled for sync",
        127 => "user on new mailbox cannot sync",
        128 => "user on legacy mailbox cannot sync",
        129 => "device is blocked for this user",
        130 => "access denied",
        131 => "account disabled",
        132 => "sync state not found",
        133 => "sync state locked",
        134 => "sync state corrupt",
        135 => "sync state already exists",
        136 => "sync state version invalid",
        137 => "command not supported",
        138 => "version not supported",
        139 => "device not fully provisionable",
        140 => "remote wipe requested",
        141 => "legacy device on strict policy",
        142 => "device not provisioned",
        143 => "policy refresh required",
        144 => "invalid policy key",
        145 => "externally managed devices not allowed",
        146 => "no recurrence in calendar",
        147 => "unexpected item class",
        148 => "remote server has no SSL",
        149 => "invalid stored request",
        150 => "item not found",
        151 => "too many folders",
        152 => "no folders found",
        153 => "items lost after move",
        154 => "failure in move operation",
        155 => "move command disallowed for non-persistent move action",
        156 => "move command invalid destination folder",
        160 => "availability: too many recipients",
        161 => "availability: DL limit reached",
        162 => "availability: transient failure",
        163 => "availability: failure",
        164 => "body part preference type not supported",
        165 => "device information required — send Settings DeviceInformation first",
        166 => "invalid account ID",
        167 => "account send disabled",
        168 => "IRM feature disabled",
        169 => "IRM transient error",
        170 => "IRM permanent error",
        171 => "IRM invalid template ID",
        172 => "IRM operation not permitted",
        173 => "no picture",
        174 => "picture too large",
        175 => "picture limit reached",
        176 => "body part conversation too large",
        177 => "maximum devices reached",
        _ => return None,
    })
}

/// Best-effort read of a command response's top-level `<Status>` value.
/// Covers the command pages we speak; unknown pages (e.g. GetItemEstimate —
/// its Status lives nested inside `<Response>`, not as a direct root child,
/// so there is deliberately no page-6 arm) yield `None`, as do
/// missing/non-numeric Status elements — callers treat `None` as "no
/// top-level status information" and behave as before.
///
/// Status-token table per the code_pages/ and tags/ tables (spec values). The
/// ItemOperations entry uses `tags::item_operations::STATUS` (0x0D) — the
/// old file-local `IO_STATUS` constant (0x0A, actually `Total` on page 20)
/// was off-spec and has been deleted along with the rest of the IO_* block.
pub fn top_level_status(root: &WbxmlElement) -> Option<u32> {
    let status_token = match root.page {
        PAGE_AIRSYNC => AS_STATUS,
        PAGE_FOLDER => FH_STATUS,
        PAGE_PING => PING_STATUS,
        PAGE_ITEM_OPS => tags::item_operations::STATUS,
        PAGE_COMPOSE => compose::STATUS,
        _ => return None,
    };
    root.children
        .iter()
        .find(|c| c.page == root.page && c.token == status_token)
        .and_then(text_value_opt)
        .and_then(|s| s.parse().ok())
}

fn parse_folder_changes(
    changes: &WbxmlElement,
    result: &mut FolderSyncResult,
) -> Result<(), WbxmlError> {
    for child in &changes.children {
        match (child.page, child.token) {
            (PAGE_FOLDER, FH_ADD | FH_UPDATE) => {
                let folder = parse_folder_element(child)?;
                result.changes.push(folder);
            }
            (PAGE_FOLDER, FH_DELETE) => {
                // Per [MS-ASFolderSync] the Delete element has a ServerId child,
                // not a text value. Be permissive: accept either form.
                let server_id = match find_child_text(child, FH_SERVER_ID) {
                    Some(s) => s,
                    None => text_value(child)?,
                };
                result.deletions.push(server_id);
            }
            // FH_COUNT (count metadata) and unknown elements: ignore
            _ => {}
        }
    }
    Ok(())
}

/// Find the first child with the given token on the same page and return its text value.
fn find_child_text(el: &WbxmlElement, token: u8) -> Option<String> {
    el.children
        .iter()
        .find(|c| c.token == token)
        .and_then(|c| match &c.value {
            WbxmlValue::Text(t) => Some(t.clone()),
            WbxmlValue::Opaque(b) => std::str::from_utf8(b)
                .ok()
                .map(std::string::ToString::to_string),
            WbxmlValue::Empty => None,
        })
}

fn parse_folder_element(folder_el: &WbxmlElement) -> Result<EasFolder, WbxmlError> {
    let mut folder = EasFolder::default();
    for child in &folder_el.children {
        match (child.page, child.token) {
            (PAGE_FOLDER, FH_SERVER_ID) => folder.server_id = text_value(child)?,
            (PAGE_FOLDER, FH_PARENT_ID) => folder.parent_id = text_value(child)?,
            (PAGE_FOLDER, FH_DISPLAY_NAME) => folder.display_name = text_value(child)?,
            (PAGE_FOLDER, FH_TYPE) => {
                let t = text_value(child)?;
                folder.class = folder_type_to_class(&t);
                folder.folder_type = t.parse::<u8>().ok();
            }
            _ => {}
        }
    }
    Ok(folder)
}

/// Map EAS folder type number (per [MS-ASFolderSync] section 2.2.3) to item class string.
/// Types 1-6, 12, 19 are mail folders; 7=tasks, 8=calendar, 9=contacts, 10=journal,
/// 11=notes. We map journal/notes to Notes for now; MVP doesn't sync them.
pub fn folder_type_to_class(type_str: &str) -> String {
    match type_str {
        "7" => "Tasks".to_string(),
        "8" => "Calendar".to_string(),
        "9" => "Contacts".to_string(),
        "10" | "11" => "Notes".to_string(),
        // Mail folder types (1-6, 12, 19) and anything unrecognized.
        _ => "Email".to_string(),
    }
}
