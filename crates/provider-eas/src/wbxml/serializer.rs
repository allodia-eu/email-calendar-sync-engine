// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// WBXML serializer. Mirrors the streaming `Serializer` class in
// `serializer.ets` — callers open a tag with `start(tag)`, write text or
// nested tags, and close it with `end()`. The tree-based helper
// `serialize_tree` takes a `WbxmlElement` and emits the full document, which
// is the typical entry point for round-trip tests and production code alike.
//
// The emitted document is a valid MS-ASWBXML byte stream:
//   `0x03 0x01 0x6A 0x00` (v1.3, no public id, UTF-8, no string table)
// followed by the element body and ending with a final `END` (0x01) token
// for the outermost element.

use crate::wbxml::{
    code_pages,
    error::{WbxmlError, WbxmlResult},
    global_tokens::{END, OPAQUE, STR_I, SWITCH_PAGE, WITH_CONTENT},
    types::{WbxmlElement, WbxmlValue},
};

/// WBXML header bytes. Constant for every MS-ASWBXML document.
const HEADER: [u8; 4] = [0x03, 0x01, 0x6A, 0x00];

/// Streaming WBXML serializer. Callers drive it via `start`, `text`,
/// `opaque`, `end`, and `done`.
///
/// The serializer is purely synchronous and owns its output `Vec<u8>`. This
/// matches the ArkTS `SyncWritableStream` contract; for async callers, wrap
/// calls in `spawn_blocking` or use `serialize_tree` which is one-shot.
pub struct Serializer {
    output: Vec<u8>,
    /// `(page, token)` of the tag waiting for its first child/value, or `None`.
    pending: Option<(u8, u8)>,
    /// Stack of open tag `(page, token)` pairs. Used only for depth tracking.
    depth: usize,
    /// The code page the serializer currently has switched to. Tracked so we
    /// only emit SWITCH_PAGE tokens when the page actually changes.
    current_page: u8,
    /// Whether the 4-byte header has been written.
    started: bool,
}

impl Default for Serializer {
    fn default() -> Self {
        Self::new()
    }
}

impl Serializer {
    /// Create a new serializer with an empty output buffer and the header
    /// already written.
    pub fn new() -> Self {
        let mut output = Vec::with_capacity(256);
        output.extend_from_slice(&HEADER);
        Self {
            output,
            pending: None,
            depth: 0,
            current_page: 0,
            started: true,
        }
    }

    /// Create a serializer that does not emit the WBXML header. Useful for
    /// tests and for composing fragments inside a larger document.
    pub fn without_header() -> Self {
        Self {
            output: Vec::new(),
            pending: None,
            depth: 0,
            current_page: 0,
            started: false,
        }
    }

    /// Reset the serializer to its initial state (header only).
    pub fn reset(&mut self) {
        self.output.clear();
        if self.started {
            self.output.extend_from_slice(&HEADER);
        }
        self.pending = None;
        self.depth = 0;
        self.current_page = 0;
    }

    /// Consume the serializer and return its output buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        self.output
    }

    /// Borrow the output buffer.
    pub fn as_bytes(&self) -> &[u8] {
        &self.output
    }

    /// Mark the document complete. Fails if there are unclosed tags or a
    /// dangling `start` without matching `end`.
    pub fn done(mut self) -> WbxmlResult<Vec<u8>> {
        if self.depth != 0 || self.pending.is_some() {
            return Err(WbxmlError::UnclosedTags);
        }
        // Trim trailing END tokens produced by the outermost `end()` —
        // the ArkTS serializer emits one END per start tag, including the
        // outermost, which the decoder also expects. Keep behavior parity.
        Ok(self.output.split_off(0))
    }

    /// Begin a tag. Must be matched by a later `end()`. The `page` is the
    /// code page index (0..26); the `token` is the tag id within that page.
    pub fn start(&mut self, page: u8, token: u8) -> WbxmlResult<&mut Self> {
        self.flush_pending(false)?;
        if !code_pages::is_valid_tag(page, token) {
            // Unknown tag — still serialize, but mark that callers may want
            // to know. The ArkTS serializer logs and continues.
            log::debug!(
                "WBXML serializer: unrecognized tag page={} token={}",
                page,
                token
            );
        }
        self.pending = Some((page, token));
        Ok(self)
    }

    /// Close the most recently opened tag.
    pub fn end(&mut self) -> WbxmlResult<&mut Self> {
        if let Some((page, token)) = self.pending.take() {
            // The tag had no children — emit as degenerated (`<Foo/>`).
            self.emit_tag(page, token, true)?;
        } else {
            // Emit an END token.
            if self.depth == 0 {
                return Err(WbxmlError::UnbalancedEnd);
            }
            self.depth -= 1;
            self.output.push(END);
        }
        Ok(self)
    }

    /// Write an empty (`<Foo/>`) tag in one call.
    pub fn empty_tag(&mut self, page: u8, token: u8) -> WbxmlResult<&mut Self> {
        self.start(page, token)?;
        self.end()?;
        Ok(self)
    }

    /// Write a tag containing an inline string value.
    pub fn data<S: AsRef<str>>(&mut self, page: u8, token: u8, value: S) -> WbxmlResult<&mut Self> {
        self.start(page, token)?;
        self.text(value)?;
        self.end()?;
        Ok(self)
    }

    /// Write an inline string (STR_I token) into the currently-open tag.
    /// Cannot be called outside of a tag context.
    pub fn text<S: AsRef<str>>(&mut self, text: S) -> WbxmlResult<&mut Self> {
        let s = text.as_ref();
        self.flush_pending(false)?;
        self.write_inline_string(s);
        Ok(self)
    }

    /// Write opaque data (OPAQUE token + mb_u_int32 length + raw bytes).
    /// Length 0 is a no-op (matches the ArkTS behavior of skipping the header).
    pub fn opaque(&mut self, data: &[u8]) -> WbxmlResult<&mut Self> {
        if data.is_empty() {
            return Ok(self);
        }
        self.flush_pending(false)?;
        self.output.push(OPAQUE);
        self.write_multibyte_uint(data.len() as u32);
        self.output.extend_from_slice(data);
        Ok(self)
    }

    // ---- internal helpers ---------------------------------------------------

    /// If there's a pending tag, emit its opening byte and clear pending.
    /// `degenerated` controls whether to set the WITH_CONTENT bit — for
    /// `end()` with no children we don't, for the first child we do.
    fn flush_pending(&mut self, degenerated: bool) -> WbxmlResult<()> {
        if let Some((page, token)) = self.pending.take() {
            self.emit_tag(page, token, degenerated)?;
            if !degenerated {
                self.depth += 1;
            }
        }
        Ok(())
    }

    /// Emit the opening byte of a tag, switching code page if necessary.
    /// Does NOT track depth — caller does that.
    fn emit_tag(&mut self, page: u8, token: u8, degenerated: bool) -> WbxmlResult<()> {
        if page != self.current_page {
            if !code_pages::is_valid_page(page) {
                return Err(WbxmlError::UnknownCodePage(page));
            }
            self.output.push(SWITCH_PAGE);
            self.output.push(page);
            self.current_page = page;
        }
        let mut byte = token & 0x3F;
        if !degenerated {
            byte |= WITH_CONTENT;
        }
        self.output.push(byte);
        Ok(())
    }

    fn write_inline_string(&mut self, s: &str) {
        self.output.push(STR_I);
        self.output.extend_from_slice(s.as_bytes());
        self.output.push(0); // NUL terminator
    }

    fn write_multibyte_uint(&mut self, mut value: u32) {
        // Encode as big-endian base-128 with continuation bit.
        // Step 1: collect bytes least-significant first.
        let mut buf = [0u8; 5];
        let mut i = 0usize;
        loop {
            buf[i] = (value & 0x7F) as u8;
            value >>= 7;
            i += 1;
            if value == 0 {
                break;
            }
        }
        // Step 2: emit in reverse order, setting the continuation bit on
        // every byte except the last.
        for (idx, b) in buf[..i].iter().enumerate().rev() {
            let last = idx == 0;
            self.output.push(if last { *b } else { *b | 0x80 });
        }
    }
}

/// Serialize a `WbxmlElement` tree into a complete WBXML document (header
/// included). Convenience wrapper around `Serializer`.
pub fn serialize_tree(root: &WbxmlElement) -> WbxmlResult<Vec<u8>> {
    let mut s = Serializer::new();
    serialize_element(&mut s, root)?;
    s.done()
}

fn serialize_element(s: &mut Serializer, el: &WbxmlElement) -> WbxmlResult<()> {
    s.start(el.page, el.token)?;
    if !el.children.is_empty() {
        for child in &el.children {
            serialize_element(s, child)?;
        }
    } else {
        match &el.value {
            WbxmlValue::Empty => {}
            WbxmlValue::Text(t) => {
                s.text(t)?;
            }
            WbxmlValue::Opaque(b) => {
                s.opaque(b)?;
            }
        }
    }
    s.end()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_emitted_on_new() {
        let s = Serializer::new();
        assert_eq!(&s.as_bytes()[..4], &HEADER);
    }

    #[test]
    fn empty_tag_serializes_as_degenerated() {
        // AirSync:SyncKey with no content → 0x0B (no WITH_CONTENT bit, no END
        // byte — the decoder sees no content bit and synthesizes the END).
        let mut s = Serializer::new();
        s.start(0, 0x0B).unwrap().end().unwrap();
        let bytes = s.done().unwrap();
        assert_eq!(&bytes[4..], &[0x0B]);
    }

    #[test]
    fn text_tag_sets_content_bit() {
        let mut s = Serializer::new();
        s.start(0, 0x0B)
            .unwrap()
            .text("abc")
            .unwrap()
            .end()
            .unwrap();
        let bytes = s.done().unwrap();
        // token 0x0B | 0x40 = 0x4B, STR_I 0x03, "abc", 0x00, END 0x01
        assert_eq!(&bytes[4..], &[0x4B, STR_I, b'a', b'b', b'c', 0x00, END]);
    }

    #[test]
    fn page_switch_is_emitted_on_change() {
        let mut s = Serializer::new();
        // AirSync:SyncKey (page 0, token 0x0B)
        s.start(0, 0x0B).unwrap().end().unwrap();
        // Contacts:Anniversary (page 1, token 0x05)
        s.start(1, 0x05).unwrap().end().unwrap();
        let bytes = s.done().unwrap();
        // After header: 0x0B (degenerated, no END) | SWITCH_PAGE 0x01 0x05 (degenerated)
        assert_eq!(&bytes[4..], &[0x0B, SWITCH_PAGE, 0x01, 0x05]);
    }

    #[test]
    fn done_rejects_unclosed_tag() {
        let mut s = Serializer::new();
        s.start(0, 0x0B).unwrap();
        assert!(matches!(s.done(), Err(WbxmlError::UnclosedTags)));
    }

    #[test]
    fn multibyte_uint_encoding_128() {
        // 128 → 0x81 0x00 (continuation + 0, then 0)
        let mut s = Serializer::without_header();
        s.write_multibyte_uint(128);
        assert_eq!(s.as_bytes(), &[0x81, 0x00]);
    }

    #[test]
    fn multibyte_uint_encoding_8192() {
        let mut s = Serializer::without_header();
        s.write_multibyte_uint(8192);
        // 8192 = 0x40 << 7 = 2^13 → 0x81 0x00 0x00? No, 8192 = 64 * 128 = 0x40_00
        // Bytes are 0x80|0x40 = 0xC0, then 0x00. So: 0xC0 0x00
        assert_eq!(s.as_bytes(), &[0xC0, 0x00]);
    }
}
