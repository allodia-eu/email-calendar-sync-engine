// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// Errors that can be returned by the WBXML codec. Modeled after the
// `ActiveSyncError` throws in the ArkTS deserializer, but flattened to a
// single enum since Rust callers prefer `Result` over exception types.

use std::fmt;

/// Errors returned by the WBXML serializer / deserializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WbxmlError {
    /// The input stream ended before a complete token could be read.
    UnexpectedEof,
    /// The input stream was empty (not even a version byte).
    EmptyStream,
    /// WBXML string tables are unsupported (MS-ASWBXML never emits them).
    StringTableUnsupported,
    /// An unknown code page index was encountered. Legal range is 0..26.
    UnknownCodePage(u8),
    /// A global token the codec does not handle (e.g. ENTITY, EXT_I_*) was seen.
    UnsupportedGlobalToken(u8),
    /// Attributes (`has_attributes` bit set) are unsupported.
    AttributesUnsupported(u8),
    /// The parser expected TEXT or OPAQUE data for a tag and saw something else.
    UnexpectedToken { expected: &'static str, got: u8 },
    /// Hit end-of-document while still inside a tag the caller was iterating.
    UnexpectedEndOfDocument,
    /// An mb_u_int32 encoding was longer than 5 bytes (malformed).
    InvalidMultibyteInteger,
    /// The caller asked the serializer to close more tags than it opened.
    UnbalancedEnd,
    /// The serializer was finalized with unclosed tags.
    UnclosedTags,
    /// Opaque data length was negative (only happens on API misuse).
    NegativeOpaqueLength,
    /// Decoding bytes as UTF-8 failed.
    InvalidUtf8,
    /// The parsed WBXML structure was valid but the content was not what the
    /// command parser expected (e.g. non-numeric status, missing required tag).
    InvalidContent(String),
    /// Expected a specific tag at the root or a child position and found a different one.
    UnexpectedTag {
        expected_page: u8,
        expected_token: u8,
        actual_page: u8,
        actual_token: u8,
    },
}

impl fmt::Display for WbxmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WbxmlError::UnexpectedEof => write!(f, "unexpected end of WBXML input"),
            WbxmlError::EmptyStream => write!(f, "WBXML input stream is empty"),
            WbxmlError::StringTableUnsupported => write!(f, "WBXML string table unsupported"),
            WbxmlError::UnknownCodePage(p) => write!(f, "unknown WBXML code page {}", p),
            WbxmlError::UnsupportedGlobalToken(t) => {
                write!(f, "unsupported WBXML global token 0x{:02X}", t)
            }
            WbxmlError::AttributesUnsupported(t) => {
                write!(f, "WBXML attributes unsupported (token 0x{:02X})", t)
            }
            WbxmlError::UnexpectedToken { expected, got } => {
                write!(
                    f,
                    "unexpected WBXML token: expected {}, got 0x{:02X}",
                    expected, got
                )
            }
            WbxmlError::UnexpectedEndOfDocument => {
                write!(f, "hit end of WBXML document unexpectedly")
            }
            WbxmlError::InvalidMultibyteInteger => {
                write!(f, "invalid WBXML multibyte integer (too many bytes)")
            }
            WbxmlError::UnbalancedEnd => write!(f, "unbalanced WBXML end() call"),
            WbxmlError::UnclosedTags => write!(f, "WBXML serializer finalised with unclosed tags"),
            WbxmlError::NegativeOpaqueLength => {
                write!(f, "negative opaque data length passed to WBXML serializer")
            }
            WbxmlError::InvalidUtf8 => write!(f, "WBXML opaque data is not valid UTF-8"),
            WbxmlError::InvalidContent(msg) => {
                write!(f, "invalid WBXML content: {}", msg)
            }
            WbxmlError::UnexpectedTag {
                expected_page,
                expected_token,
                actual_page,
                actual_token,
            } => {
                write!(
                    f,
                    "unexpected WBXML tag: expected page {} token 0x{:02X}, got page {} token 0x{:02X}",
                    expected_page, expected_token, actual_page, actual_token
                )
            }
        }
    }
}

impl std::error::Error for WbxmlError {}

impl From<std::str::Utf8Error> for WbxmlError {
    fn from(_: std::str::Utf8Error) -> Self {
        WbxmlError::InvalidUtf8
    }
}

impl From<std::string::FromUtf8Error> for WbxmlError {
    fn from(_: std::string::FromUtf8Error) -> Self {
        WbxmlError::InvalidUtf8
    }
}

pub type WbxmlResult<T> = Result<T, WbxmlError>;
