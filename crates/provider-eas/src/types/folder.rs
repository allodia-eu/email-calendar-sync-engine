// SPDX-License-Identifier: MPL-2.0
//! FolderSync folder-tree deltas and folder create/update/delete requests.

use serde::{Deserialize, Serialize};
// ---------- Folders (FolderSync) ----------

/// One folder from a FolderSync change entry ([MS-ASFD]): the server's
/// folder-tree delta, addressed by ServerId.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EasFolder {
    /// Server-assigned folder id (FolderHierarchy:ServerId) — stable across
    /// renames and moves.
    pub server_id: String,
    /// Parent folder's ServerId ("0" for the root).
    pub parent_id: String,
    /// Folder name exactly as the server stores it (locale-dependent).
    pub display_name: String,
    /// `"Email"`, `"Calendar"`, `"Contacts"`, `"Tasks"`, `"Notes"`, etc.
    pub class: String,
    /// Raw EAS folder Type byte (MS-ASFD `FolderHierarchy:Type`): 2=Inbox,
    /// 3=Drafts, 4=DeletedItems, 5=Sent, 6=Outbox, 7=Tasks, 8=Calendar,
    /// 9=Contacts, 10/11=Notes/Journal, 1/12=user-created mail, etc. Surfaced so
    /// the frontend can derive a canonical role without locale-dependent
    /// name-matching. `None` when the element is absent or non-numeric.
    #[serde(default)]
    pub folder_type: Option<u8>,
}

/// Result of the FolderSync command ([MS-ASFD] §2.2.1.1): the next sync key
/// plus the folder-tree delta since the previous one.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FolderSyncResult {
    /// Command status per [MS-ASCMD] common + FolderSync codes; 1 = success.
    /// Non-success is surfaced by the client as `EasError::CommandStatus`.
    #[serde(default)]
    pub status: u32,
    /// Updated sync key to persist for the next FolderSync call.
    pub sync_key: String,
    /// Folders added or updated since the last sync key.
    pub changes: Vec<EasFolder>,
    /// Server IDs of folders deleted since the last sync key.
    pub deletions: Vec<String>,
}

// ---------- Folder create/update/delete ----------

/// FolderCreate request ([MS-ASCMD] §2.2.1.4): create one folder under
/// `parent_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderCreateRequest {
    /// ServerId of the parent folder ("0" = root).
    pub parent_id: String,
    /// The new folder's name.
    pub display_name: String,
    /// Folder content class (`"Email"`, `"Calendar"`, `"Contacts"`,
    /// `"Tasks"`, `"Notes"`).
    pub class: String,
}

/// FolderUpdate request ([MS-ASCMD] §2.2.1.6): rename and/or move one
/// folder; `None` fields are omitted (left unchanged).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderUpdateRequest {
    /// ServerId of the folder to update.
    pub server_id: String,
    /// New parent ServerId ("0" = root), when moving.
    pub parent_id: Option<String>,
    /// New display name, when renaming.
    pub display_name: Option<String>,
}

/// FolderDelete request ([MS-ASCMD] §2.2.1.3): delete one folder (and its
/// subtree) by ServerId.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderDeleteRequest {
    /// ServerId of the folder to delete.
    pub server_id: String,
}

// NOTE: The legacy `pub struct EasError { status, message, command }` that
// previously lived here was dead code — every live EAS error in the codebase
// flows through `crate::client::EasError` (the `thiserror` enum declared
// in `client.rs`). It was removed in Phase 3b Task 1. If you need to surface
// an EAS error, use `client::EasError`.
