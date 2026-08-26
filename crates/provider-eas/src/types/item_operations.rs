// SPDX-License-Identifier: MPL-2.0
//! ItemOperations Fetch / EmptyFolderContents / Move request-response types.

use serde::{Deserialize, Serialize};
// ---------- ItemOperations ----------

/// ItemOperations → Fetch request ([MS-ASCMD] §2.2.1.10): one item body or
/// attachment, addressed by ServerId, FileReference, or search LongId.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemOperationsFetchRequest {
    /// Server ID of the item to fetch.
    pub server_id: String,
    /// Collection (folder) ID containing the item.
    pub collection_id: String,
    /// For attachment fetches: the FileReference returned in a prior Sync.
    pub file_reference: Option<String>,
    /// For search-result fetches: the `search:LongId` returned in a prior
    /// Search response ([MS-ASCMD] §2.2.3.98.1 / example §4.10.3.3). When
    /// set (and `file_reference` is not), the request is the LongId form:
    /// Store=Mailbox + search:LongId + Options>BodyPreference, with no
    /// CollectionId/ServerId. Precedence in the builder:
    /// `file_reference` > `long_id` > collection/server-id.
    /// `#[serde(default)]` keeps pre-Task-5 IPC payloads (no `longId` key)
    /// deserializing unchanged.
    #[serde(default)]
    pub long_id: Option<String>,
    /// Fetch the item as raw MIME instead of an HTML body (task 3). When
    /// true, the item-fetch branch emits `airsync:MIMESupport` level 2
    /// ("send MIME data for all messages", [MS-ASCMD] §2.2.3.110.3) BEFORE
    /// `airsyncbase:BodyPreference` and switches the BodyPreference Type to
    /// 4 — the §4.10.2.1 request shape. Level 2 (not the spec example's 1):
    /// a MIME fetch wants the raw message for ALL items, not S/MIME-only.
    /// Ignored on the attachment (`file_reference`) and search (`long_id`)
    /// branches — MIME doesn't apply to attachment fetches, and the LongId
    /// shape is kept minimal until a consumer needs it. `#[serde(default)]`
    /// keeps pre-Task-3 IPC payloads (no `mime` key) deserializing unchanged.
    #[serde(default)]
    pub mime: bool,
    /// Opt in to a multipart response ([MS-ASCMD] §2.2.1.10.1): when true,
    /// the ItemOperations POST carries the `MS-ASAcceptMultiPart: T` header
    /// ([MS-ASHTTP] §2.2.1.1.2.5) and the client accepts a
    /// `application/vnd.ms-sync.multipart` body — large payloads arrive as
    /// binary parts referenced by `itemoperations:Part` instead of inline
    /// base64, so a big attachment/body never has to round-trip through the
    /// WBXML string table. The server MAY still answer plain WBXML; both
    /// shapes are handled. `#[serde(default)]` keeps pre-multipart IPC
    /// payloads (no `accept_multipart` key) on the inline path unchanged.
    #[serde(default)]
    pub accept_multipart: bool,
}

/// Result of an ItemOperations → Fetch: the inline payload plus its
/// content type and status.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ItemOperationsFetchResult {
    /// `itemoperations:Status` (1 = success).
    pub status: u8,
    /// Raw base64-encoded bytes for attachment fetches, or item fields for item fetches.
    pub data: Option<String>,
    /// `airsyncbase:ContentType` of the fetched data, when present.
    pub content_type: Option<String>,
}

// ---------- ItemOperations EmptyFolderContents ----------

/// ItemOperations → EmptyFolderContents request ([MS-ASCMD] §4.14.4.1):
/// deletes EVERY item in the named folder server-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmptyFolderContentsRequest {
    /// Collection (folder) ID whose contents are deleted
    /// (`airsync:CollectionId` inside `EmptyFolderContents`).
    pub collection_id: String,
    /// When true the request carries `Options>DeleteSubFolders` and the
    /// folder's subfolders are deleted too. When false the Options element
    /// is omitted entirely (the server default keeps subfolders).
    #[serde(default)]
    pub delete_sub_folders: bool,
}

fn default_empty_folder_contents_status() -> u32 {
    1
}

/// Result of ItemOperations → EmptyFolderContents ([MS-ASCMD] §4.14.4.2):
/// status pair plus the CollectionId echo confirming which folder was
/// emptied.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmptyFolderContentsResult {
    /// Effective command status. Starts at the top-level itemoperations:Status
    /// (1 = success, the default when the element is absent — mirroring
    /// GetItemEstimate); an EmptyFolderContents-level Status, when present,
    /// overrides it (more specific wins — the ItemOperations rule).
    #[serde(default = "default_empty_folder_contents_status")]
    pub status: u32,
    /// EmptyFolderContents-level itemoperations:Status, `None` when the
    /// element is absent (e.g. a command-level rejection carries no
    /// Response at all).
    #[serde(default)]
    pub empty_status: Option<u32>,
    /// The `airsync:CollectionId` echoed back by the server — confirmation
    /// of the folder whose contents were deleted.
    #[serde(default)]
    pub collection_id: Option<String>,
}

// ---------- ItemOperations Move (conversation move) ----------

/// ItemOperations → Move request ([MS-ASCMD] §4.25.1): moves a whole
/// conversation to another folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMoveRequest {
    /// Destination folder's server id (`itemoperations:DstFldId`).
    pub dst_folder_id: String,
    /// The conversation to move (`itemoperations:ConversationId`) — OPAQUE
    /// server bytes carried verbatim (never base64-decoded or re-encoded),
    /// the same convention as the sync path's `conversation_id`. Over IPC
    /// this serializes as a byte array.
    pub conversation_id: Vec<u8>,
    /// When true the request carries `Options>MoveAlways` and ALL FUTURE
    /// messages of the conversation are moved to the destination folder
    /// too — not just the ones currently on the server. Callers MUST
    /// surface this consequence to the user (it behaves like a persistent
    /// server-side rule). When false the Options element is omitted
    /// entirely.
    #[serde(default)]
    pub move_always: bool,
}

fn default_conversation_move_status() -> u32 {
    1
}

/// Result of ItemOperations → Move ([MS-ASCMD] §4.25.2): status pair plus
/// the ConversationId echo confirming which conversation was moved.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationMoveResult {
    /// Effective command status. Starts at the top-level itemoperations:Status
    /// (1 = success, the default when the element is absent — mirroring
    /// GetItemEstimate); a Move-level Status, when present, overrides it
    /// (more specific wins — the ItemOperations rule).
    #[serde(default = "default_conversation_move_status")]
    pub status: u32,
    /// Move-level itemoperations:Status, `None` when the element is absent
    /// (e.g. a command-level rejection carries no Response at all).
    #[serde(default)]
    pub move_status: Option<u32>,
    /// The `itemoperations:ConversationId` echoed back by the server —
    /// opaque bytes verbatim (accepted in both the opaque-binary and
    /// base64-text wire forms, never decoded), confirming the conversation
    /// that was moved. `None` when absent or empty.
    #[serde(default)]
    pub conversation_id: Option<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-multipart IPC payloads carry no `accept_multipart` key; the serde
    /// default must read them as `false` so those requests go out WITHOUT the
    /// `MS-ASAcceptMultiPart: T` header and keep getting inline (base64 Data)
    /// responses — wire behavior unchanged for existing callers.
    #[test]
    fn item_operations_fetch_without_accept_multipart_defaults_false() {
        let json = r#"{"server_id":"5:1","collection_id":"5","file_reference":null}"#;
        let req: ItemOperationsFetchRequest = serde_json::from_str(json).expect("deserialize");
        assert!(
            !req.accept_multipart,
            "absent `accept_multipart` must default to false (no MS-ASAcceptMultiPart header)"
        );
    }
}
