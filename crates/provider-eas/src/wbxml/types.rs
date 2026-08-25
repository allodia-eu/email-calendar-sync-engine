// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// Domain model for the WBXML codec. The ArkTS source uses an event-based
// streaming API (serializer.start(tag) / serializer.end() / deserializer.nextTag()).
// We mirror that streaming API on the serializer side for parity, but the
// deserializer also exposes a tree-based API (`deserialize_to_tree`) which is
// what round-trip tests want. The tree form is also a more natural fit for
// Rust (no borrowed slices across iterator boundaries).

use serde::{Deserialize, Serialize};

use crate::wbxml::{code_pages, global_tokens};

/// A WBXML element. Either a container for children, or a leaf holding a value.
///
/// Tags are identified by `(page, token)` rather than a combined `u16` — this
/// avoids the bit-fiddling that the ArkTS code does (`page << 6 | token`) and
/// makes debug output more readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WbxmlElement {
    /// Code page index (0..26).
    pub page: u8,
    /// Token id within `page` (low 6 bits, no content/attr flags).
    pub token: u8,
    /// Children, in document order. Empty for leaf elements.
    pub children: Vec<WbxmlElement>,
    /// Leaf value. Populated only when `children` is empty and a value was read.
    /// Can be either an inline string (STR_I) or decoded-from-opaque bytes.
    pub value: WbxmlValue,
}

/// Element value — mirrors the three wire forms MS-ASWBXML uses for leaf data:
/// empty (`<Foo/>`), inline string (`<Foo>bar</Foo>`), or opaque
/// (`<Foo>\xC3\x03abc</Foo>`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WbxmlValue {
    /// Element had no content (`<Foo/>`).
    #[default]
    Empty,
    /// Inline string token (STR_I). Always UTF-8.
    Text(String),
    /// Opaque data token (OPAQUE). May or may not be valid UTF-8.
    /// Stored as bytes because MS-ASWBXML uses OPAQUE for binary blobs
    /// (Certificates, ConversationId, etc.) as well as large text payloads.
    Opaque(Vec<u8>),
}

impl WbxmlElement {
    /// Construct a leaf element with no value (`<Foo/>`).
    pub fn empty(page: u8, token: u8) -> Self {
        Self {
            page,
            token,
            children: Vec::new(),
            value: WbxmlValue::Empty,
        }
    }

    /// Construct a leaf element with a text value.
    pub fn text<S: Into<String>>(page: u8, token: u8, text: S) -> Self {
        Self {
            page,
            token,
            children: Vec::new(),
            value: WbxmlValue::Text(text.into()),
        }
    }

    /// Construct a leaf element with opaque binary data.
    pub fn opaque(page: u8, token: u8, data: Vec<u8>) -> Self {
        Self {
            page,
            token,
            children: Vec::new(),
            value: WbxmlValue::Opaque(data),
        }
    }

    /// Construct a container element with the given children.
    pub fn container(page: u8, token: u8, children: Vec<WbxmlElement>) -> Self {
        Self {
            page,
            token,
            children,
            value: WbxmlValue::Empty,
        }
    }

    /// Add a child to this element, returning `&mut Self` for chaining.
    #[must_use]
    pub fn with_child(mut self, child: WbxmlElement) -> Self {
        self.children.push(child);
        self
    }

    /// Return the tag name of this element, or `"unknown"` if the token
    /// is not registered on its code page. An unregistered token almost
    /// always means a misdecode (wrong page after SWITCH_PAGE, a fuzzed
    /// document) or a missing token constant — exactly the class that once
    /// made every Sync `<Delete>` parse as an empty id — so it is logged
    /// (rate-limited by the caller's own log config; a malformed response
    /// floods only the failing command's log).
    pub fn tag_name(&self) -> &'static str {
        if let Some(n) = code_pages::code_page(self.page).and_then(|p| p.tag_name(self.token)) {
            n
        } else {
            log::warn!(
                "[wbxml] unregistered token 0x{:02X} on code page {} — possible misdecode or missing tag constant",
                self.token,
                self.page
            );
            "unknown"
        }
    }

    /// Return `true` if this is a global token (page 0 + token < 5).
    pub fn is_global(&self) -> bool {
        global_tokens::is_global_token(self.token)
    }
}
