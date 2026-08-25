// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `ItemOperations` (`ItemOperations` page-20 token 0x05).
pub const ITEM_OPERATIONS: u8 = 0x05;
/// `Fetch` (`ItemOperations` page-20 token 0x06).
pub const FETCH: u8 = 0x06;
/// `Store` (`ItemOperations` page-20 token 0x07).
pub const STORE: u8 = 0x07;
/// `Options` (`ItemOperations` page-20 token 0x08).
pub const OPTIONS: u8 = 0x08;
/// `Range` (`ItemOperations` page-20 token 0x09).
pub const RANGE: u8 = 0x09;
/// `Total` (`ItemOperations` page-20 token 0x0a).
pub const TOTAL: u8 = 0x0A;
/// `Properties` (`ItemOperations` page-20 token 0x0b).
pub const PROPERTIES: u8 = 0x0B;
/// `Data` (`ItemOperations` page-20 token 0x0c).
pub const DATA: u8 = 0x0C;
/// `Status` (`ItemOperations` page-20 token 0x0d).
pub const STATUS: u8 = 0x0D;
/// `Response` (`ItemOperations` page-20 token 0x0e).
pub const RESPONSE: u8 = 0x0E;
/// Part element ([MS-ASCMD] §2.2.3.130): multipart responses only.
/// Child of airsyncbase:Body (or Properties for document-library
/// fetches); its integer text is the index of the MultiPartResponse
/// part carrying the payload, replacing the inline Data element.
pub const PART: u8 = 0x11;
/// EmptyFolderContents request/response element ([MS-ASCMD] §4.14.4).
pub const EMPTY_FOLDER_CONTENTS: u8 = 0x12;
/// Child of Options: also delete the folder's subfolders.
pub const DELETE_SUB_FOLDERS: u8 = 0x13;
/// Move request/response element — conversation move ([MS-ASCMD]
/// §4.25). NOT the MoveItems-page (5) Move token; this is the
/// ItemOperations-namespace Move.
pub const MOVE: u8 = 0x16;
/// Destination folder id, child of Move.
pub const DST_FLD_ID: u8 = 0x17;
/// Conversation id, child of Move — opaque binary on the wire.
pub const CONVERSATION_ID: u8 = 0x18;
/// Child of Options inside Move: also move all FUTURE messages of the
/// conversation.
pub const MOVE_ALWAYS: u8 = 0x19;
