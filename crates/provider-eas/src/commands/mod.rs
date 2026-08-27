// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// Pure WBXML marshalers for the 9 MVP EAS commands. Each command has:
//   - `build_*_request(input) -> WbxmlElement` — build the request tree
//   - `parse_*_response(tree) -> Result<output, WbxmlError>` — parse the response tree
//
// HTTP transport lives in `client/` (Phase 8) and wraps these in POST requests.
//
// Code-page tag constants below are deliberately exhaustive: they mirror the
// EAS protocol spec so command builders can reach for any tag without
// re-checking the reference. Constants not wired into the MVP
// request/response builders yet are re-exported as part of the crate's
// protocol reference surface rather than kept private-and-dead.

pub use crate::{
    types::*,
    wbxml::{
        WbxmlError,
        tags::{self, compose, pages},
        types::{WbxmlElement, WbxmlValue},
    },
};

// ---------- Code page indices (for readability) ----------

/// `AirSync` code-page index.
pub const PAGE_AIRSYNC: u8 = 0;
/// `FolderHierarchy` code-page index.
pub const PAGE_FOLDER: u8 = 7;
/// `Ping` code-page index.
pub const PAGE_PING: u8 = 13;
/// `ItemOperations` code-page index.
pub const PAGE_ITEM_OPS: u8 = 20;
/// `ComposeMail` code-page index.
pub const PAGE_COMPOSE: u8 = 21;

// ---------- AirSync (page 0) tag ids ----------

/// `Sync` (`AirSync` page-0 token 0x05).
pub const AS_SYNC: u8 = 0x05;
/// `Responses` ([MS-ASWBXML] §2.1.2.1.1 page-0 table) — container for the
/// per-item `Add`/`Change` statuses a server MAY send on an upsync Sync
/// response ([MS-ASSYNC] §2.2.2), echoing the request's ClientIds. Public
/// for the M8 calendar upsync Task 3 response parser (visibility widened
/// ahead of use — the M8 precedent).
pub const AS_RESPONSES: u8 = 0x06;
/// `Add` (`AirSync` page-0 token 0x07).
pub const AS_ADD: u8 = 0x07;
/// `Change` (`AirSync` page-0 token 0x08).
pub const AS_CHANGE: u8 = 0x08;
/// `Delete` (`AirSync` page-0 token 0x09).
pub const AS_DELETE: u8 = 0x09;
/// `SyncKey` (`AirSync` page-0 token 0x0b).
pub const AS_SYNC_KEY: u8 = 0x0B;
/// `ClientId` ([MS-ASWBXML] §2.1.2.1.1 page-0 table) — the client-generated
/// correlation id of a Sync `Add` command; the server echoes it under
/// `Responses` so the caller can map its new ServerId ([MS-ASCMD] caps the
/// value at 40 chars — see `types::new_calendar_client_id`).
pub const AS_CLIENT_ID: u8 = 0x0C;
/// `ServerId` (`AirSync` page-0 token 0x0d).
pub const AS_SERVER_ID: u8 = 0x0D;
/// `Status` (`AirSync` page-0 token 0x0e).
pub const AS_STATUS: u8 = 0x0E;
/// `Collection` (`AirSync` page-0 token 0x0f).
pub const AS_COLLECTION: u8 = 0x0F;
/// `Collections` (`AirSync` page-0 token 0x1c).
pub const AS_COLLECTIONS: u8 = 0x1C;
/// `Class` (`AirSync` page-0 token 0x10).
pub const AS_CLASS: u8 = 0x10;
/// `CollectionId` (`AirSync` page-0 token 0x12).
pub const AS_COLLECTION_ID: u8 = 0x12;
/// `GetChanges` (`AirSync` page-0 token 0x13).
pub const AS_GET_CHANGES: u8 = 0x13;
/// `MoreAvailable` (`AirSync` page-0 token 0x14).
pub const AS_MORE_AVAILABLE: u8 = 0x14;
/// `WindowSize` (`AirSync` page-0 token 0x15).
pub const AS_WINDOW_SIZE: u8 = 0x15;
/// `Commands` (`AirSync` page-0 token 0x16).
pub const AS_COMMANDS: u8 = 0x16;
/// `Options` (`AirSync` page-0 token 0x17).
pub const AS_OPTIONS: u8 = 0x17; // Options (per [MS-ASSYNC] 2.2.3.25); matches tags::airsync::OPTIONS
const AS_FILTER_TYPE: u8 = 0x18; // FilterType (per [MS-ASWBXML] §2.1.2.1.1 page-0 table; 0x11 is unassigned)
const AS_DELETES_AS_MOVES: u8 = 0x1E; // DeletesAsMoves (per [MS-ASWBXML] §2.1.2.1.1 page-0 table; verified in code_pages/pages_00_09.rs AIRSYNC_TOKENS)
const AS_SUPPORTED: u8 = 0x20; // Supported (per [MS-ASWBXML] §2.1.2.1.1 page-0 table; verified in code_pages/pages_00_09.rs AIRSYNC_TOKENS)
const AS_MIME_SUPPORT: u8 = 0x22; // MIMESupport (per [MS-ASWBXML] §2.1.2.1.1 page-0 table; verified in code_pages/pages_00_09.rs AIRSYNC_TOKENS)
const AS_MIME_TRUNCATION: u8 = 0x23; // MIMETruncation (same page-0 table)
/// `ApplicationData` (`AirSync` page-0 token 0x1d).
pub const AS_APPLICATION_DATA: u8 = 0x1D;

// ---------- FolderHierarchy (page 7) tag ids ----------

/// `DisplayName` (`FolderHierarchy` page-7 token 0x07).
pub const FH_DISPLAY_NAME: u8 = 0x07;
/// `ServerId` (`FolderHierarchy` page-7 token 0x08).
pub const FH_SERVER_ID: u8 = 0x08;
/// `ParentId` (`FolderHierarchy` page-7 token 0x09).
pub const FH_PARENT_ID: u8 = 0x09;
/// `Type` (`FolderHierarchy` page-7 token 0x0a).
pub const FH_TYPE: u8 = 0x0A;
/// `Status` (`FolderHierarchy` page-7 token 0x0c).
pub const FH_STATUS: u8 = 0x0C;
/// `Changes` (`FolderHierarchy` page-7 token 0x0e).
pub const FH_CHANGES: u8 = 0x0E;
/// `Add` (`FolderHierarchy` page-7 token 0x0f).
pub const FH_ADD: u8 = 0x0F;
/// `Delete` (`FolderHierarchy` page-7 token 0x10).
pub const FH_DELETE: u8 = 0x10;
/// `Update` (`FolderHierarchy` page-7 token 0x11).
pub const FH_UPDATE: u8 = 0x11;
/// `SyncKey` (`FolderHierarchy` page-7 token 0x12).
pub const FH_SYNC_KEY: u8 = 0x12;
/// `FolderCreate` (`FolderHierarchy` page-7 token 0x13).
pub const FH_FOLDER_CREATE: u8 = 0x13;
/// `FolderDelete` (`FolderHierarchy` page-7 token 0x14).
pub const FH_FOLDER_DELETE: u8 = 0x14;
/// `FolderUpdate` (`FolderHierarchy` page-7 token 0x15).
pub const FH_FOLDER_UPDATE: u8 = 0x15;
/// `FolderSync` (`FolderHierarchy` page-7 token 0x16).
pub const FH_FOLDER_SYNC: u8 = 0x16;

// ---------- Ping (page 13) tag ids ----------

/// `Ping` (`Ping` page-13 token 0x05).
pub const PING_PING: u8 = 0x05;
/// `Status` (`Ping` page-13 token 0x07).
pub const PING_STATUS: u8 = 0x07;
/// `HeartbeatInterval` (`Ping` page-13 token 0x08).
pub const PING_HEARTBEAT_INTERVAL: u8 = 0x08;
/// `Folders` (`Ping` page-13 token 0x09).
pub const PING_FOLDERS: u8 = 0x09;
/// `Folder` (`Ping` page-13 token 0x0a).
pub const PING_FOLDER: u8 = 0x0A;
const PING_ID: u8 = 0x0B;
const PING_CLASS: u8 = 0x0C;

// ---------- ItemOperations (page 20) tag ids ----------
//
// NOTE: ItemOperations tag constants live in `crate::wbxml::tags::item_operations`
// (ITEM_OPERATIONS, FETCH, STORE, OPTIONS, RANGE, TOTAL, PROPERTIES, DATA,
// STATUS, RESPONSE), which matches [MS-ASWBXML] §2.1.2.1.21 exactly. The local
// `IO_*` constants that used to live here were WRONG and have been deleted:
//   - IO_RESPONSE      = 0x08  // 0x08 is actually Options;   correct Response = 0x0E
//   - IO_STATUS        = 0x0A  // 0x0A is actually Total;     correct Status   = 0x0D
//   - IO_COLLECTION_ID = 0x0B  // page-20 0x0B is Properties; CollectionId is the AirSync-page (0)
//     token 0x12 (MS-ASCMD §6.23)
//   - IO_SERVER_ID     = 0x0C  // page-20 0x0C is Data;       ServerId is the AirSync-page (0)
//     token 0x0D
//   - IO_FILE_REFERENCE= 0x0D  // page-20 0x0D is Status;     FileReference is the AirSyncBase-page
//     (17) token 0x11
//   - IO_PROPERTIES    = 0x0F  // 0x0F is actually Version;   correct Properties = 0x0B
//   - IO_DATA          = 0x10  // 0x10 is actually Schema;    correct Data     = 0x0C
//   - IO_CONTENT_TYPE  = 0x12  // page-20 0x12 is EmptyFolderContents; the fetch response
//     ContentType is airsyncbase:ContentType (page 17, 0x17) per MS-ASCMD §2.2.3.139.2
// All builders/parsers in this file now reach for `tags::item_operations::*`
// (plus `tags::base::*` / the AirSync-page `AS_*` constants) directly.

// ---------- ComposeMail (page 21) tag ids ----------
//
// NOTE: ComposeMail tag constants now live in `crate::wbxml::tags::compose`
// (SEND_MAIL, SMART_FORWARD, SMART_REPLY, SAVE_IN_SENT_ITEMS, REPLACE_MIME,
// SOURCE, FOLDER_ID, ITEM_ID, LONG_ID, MIME, CLIENT_ID, STATUS). The local
// `CM_*` aliases that used to live here were WRONG:
//   - CM_MIME        = 0x09  // 0x09 is actually ReplaceMime; correct MIME = 0x10
//   - CM_REPLACE_MIME = 0x0E // 0x0E is actually LongId; correct ReplaceMime = 0x09
//   - CM_STATUS      = 0x18  // 0x18 is not in the page; correct Status = 0x12
// They have been deleted. All builders/parsers in this file now reach for the
// `tags::compose::*` constants directly so the wire bytes match [MS-ASCMD].

/// Distinct from the module-level `text_value(&WbxmlElement) -> Result<String,
/// WbxmlError>` helper (which is the fallible form used by the strict FolderSync
/// / Sync-key parsers). This `_opt` variant is the permissive form for
/// `ApplicationData` field extraction, where a missing or non-text value should
/// silently map to `None` rather than abort the whole item parse.
fn text_value_opt(elem: &WbxmlElement) -> Option<String> {
    match &elem.value {
        WbxmlValue::Text(s) => Some(s.clone()),
        WbxmlValue::Opaque(b) => std::str::from_utf8(b)
            .ok()
            .map(std::string::ToString::to_string),
        WbxmlValue::Empty => None,
    }
}
/// Standard base64 encoding for opaque attachment bytes.
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
fn expect_tag(el: &WbxmlElement, expected_page: u8, expected_token: u8) -> Result<(), WbxmlError> {
    if el.page != expected_page || el.token != expected_token {
        return Err(WbxmlError::UnexpectedTag {
            expected_page,
            expected_token,
            actual_page: el.page,
            actual_token: el.token,
        });
    }
    Ok(())
}

fn text_value(el: &WbxmlElement) -> Result<String, WbxmlError> {
    match &el.value {
        WbxmlValue::Text(t) => Ok(t.clone()),
        WbxmlValue::Opaque(b) => String::from_utf8(b.clone()).map_err(|_| {
            WbxmlError::InvalidContent(format!("tag {} had non-UTF-8 opaque value", el.tag_name()))
        }),
        WbxmlValue::Empty => Ok(String::new()),
    }
}

mod folder_sync;
pub use folder_sync::*;
mod sync;
pub use sync::*;
mod send;
pub use send::*;
mod item_operations;
pub use item_operations::*;
mod ping;
pub use ping::*;
mod settings;
pub use settings::*;
mod folder_ops;
pub use folder_ops::*;
mod meeting;
pub use meeting::*;
mod search;
pub use search::*;
mod validate_cert;
pub use validate_cert::*;
mod resolve_recipients;
pub use resolve_recipients::*;

#[doc(hidden)]
pub mod tests_common {
    use super::{WbxmlElement, WbxmlError};
    use crate::wbxml::{deserialize_to_tree, serialize_tree};

    pub fn round_trip(root: &WbxmlElement) -> WbxmlElement {
        let bytes = serialize_tree(root).expect("serialize");
        deserialize_to_tree(&bytes).expect("deserialize")
    }

    pub fn text_value(el: &WbxmlElement) -> Result<String, WbxmlError> {
        super::text_value(el)
    }

    pub fn base64_encode(bytes: &[u8]) -> String {
        super::base64_encode(bytes)
    }
}
