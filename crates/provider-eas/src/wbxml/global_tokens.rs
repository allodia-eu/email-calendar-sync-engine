// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// WBXML global tokens as defined in [MS-ASWBXML] section 2.2. These tokens
// have special meaning to the WBXML parser itself (switch page, end element,
// inline string, opaque data, …). Element tags must avoid the low 5 bits
// where these tokens live.

/// Switch code page. Followed by a single `u8` indicating the new code page.
pub const SWITCH_PAGE: u8 = 0x00;

/// End of an attribute list or an element.
pub const END: u8 = 0x01;

/// A character entity. Followed by an `mb_u_int32` encoding the character
/// entity number. Not used by MS-ASWBXML; the codec rejects it.
pub const ENTITY: u8 = 0x02;

/// Inline string. Followed by a UTF-8, NUL-terminated string.
pub const STR_I: u8 = 0x03;

/// A literal tag or attribute name. Followed by an `mb_u_int32` offset into
/// the string table. MS-ASWBXML never uses string tables, so this is rejected.
pub const LITERAL: u8 = 0x04;

/// First tag token usable by code pages. Tokens `< TAG_BASE` are global.
pub const TAG_BASE: u8 = 0x05;

/// Bit set on a tag token when the element has content (children or a value).
pub const WITH_CONTENT: u8 = 0x40;

/// Bit set on a tag token when the element has attributes. MS-ASWBXML never
/// emits attributes; the codec rejects this bit.
pub const WITH_ATTRIBUTES: u8 = 0x80;

/// Opaque data token. Followed by an `mb_u_int32` length and that many bytes.
pub const OPAQUE: u8 = 0xC3;

/// Returns `true` if `token` is one of the global tokens (`0x00..=0x04`).
/// These tokens cannot be used as element ids.
#[inline]
pub fn is_global_token(token: u8) -> bool {
    // Only the raw token value (low bits) counts here — callers pass the
    // already-masked id. Any token < TAG_BASE is global.
    token < TAG_BASE
}
