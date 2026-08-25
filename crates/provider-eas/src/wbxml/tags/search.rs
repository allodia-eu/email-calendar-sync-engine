// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `Search` code-page index.
pub const PAGE: u8 = 15;
/// `Search` (`Search` page-15 token 0x05).
pub const SEARCH: u8 = 0x05;
/// `Store` (`Search` page-15 token 0x07).
pub const STORE: u8 = 0x07;
/// `Name` (`Search` page-15 token 0x08).
pub const NAME: u8 = 0x08;
/// `Query` (`Search` page-15 token 0x09).
pub const QUERY: u8 = 0x09;
/// `Options` (`Search` page-15 token 0x0a).
pub const OPTIONS: u8 = 0x0A;
/// `Range` (`Search` page-15 token 0x0b).
pub const RANGE: u8 = 0x0B;
/// `Status` (`Search` page-15 token 0x0c).
pub const STATUS: u8 = 0x0C;
/// `Response` (`Search` page-15 token 0x0d).
pub const RESPONSE: u8 = 0x0D;
/// `Result` (`Search` page-15 token 0x0e).
pub const RESULT: u8 = 0x0E;
/// `Properties` (`Search` page-15 token 0x0f).
pub const PROPERTIES: u8 = 0x0F;
/// `Total` (`Search` page-15 token 0x10).
pub const TOTAL: u8 = 0x10;
/// `And` (`Search` page-15 token 0x13).
pub const AND: u8 = 0x13;
/// `FreeText` (`Search` page-15 token 0x15).
pub const FREE_TEXT: u8 = 0x15;
/// `DeepTraversal` (`Search` page-15 token 0x17).
pub const DEEP_TRAVERSAL: u8 = 0x17;
/// `LongId` (`Search` page-15 token 0x18).
pub const LONG_ID: u8 = 0x18;
/// `RebuildResults` (`Search` page-15 token 0x19).
pub const REBUILD_RESULTS: u8 = 0x19;
