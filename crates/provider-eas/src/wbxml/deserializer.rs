// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// WBXML deserializer. Two entry points:
//
//   * `deserialize_to_tree(bytes)` — parses a complete document into a `WbxmlElement` tree. The
//     recommended API for new code.
//   * `Deserializer::new(bytes)` — low-level, event-style iterator that mirrors the ArkTS
//     `nextTag()` API. Used when you need to drive parsing without allocating a full tree (e.g.
//     streaming into a typed struct).
//
// Only the subset of WBXML that MS-ASWBXML uses is supported. The decoder
// rejects string tables, entities, processing instructions, and attributes
// (all unsupported by MS-ASWBXML) with a `WbxmlError`.

use crate::wbxml::{
    code_pages,
    error::{WbxmlError, WbxmlResult},
    global_tokens::{END, ENTITY, LITERAL, OPAQUE, STR_I, SWITCH_PAGE, WITH_ATTRIBUTES},
    types::{WbxmlElement, WbxmlValue},
};

/// Event type emitted by the low-level deserializer. Mirrors the constants on
/// the ArkTS `Deserializer` class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeserializerEvent {
    /// A start tag was seen. The element's `(page, token)` is on the decoder.
    Start,
    /// An end tag (or the implicit close of a no-content tag) was seen.
    End,
    /// Inline string or opaque data for the current element.
    Text,
    /// OPAQUE data for the current element (length-prefixed bytes).
    Opaque,
    /// The document has been fully consumed.
    Done,
}

/// Low-level WBXML deserializer. Operates on a `&[u8]` slice.
#[derive(Debug)]
pub struct Deserializer<'a> {
    input: &'a [u8],
    pos: usize,
    /// Stack of `(page, token)` for open elements.
    open_stack: Vec<(u8, u8)>,
    /// The current code page (starts at 0 per MS-ASWBXML).
    current_page: u8,
    /// The most recent start tag's `(page, token)`.
    current_tag: (u8, u8),
    /// Whether `current_tag` was emitted as no-content (`<Foo/>`).
    no_content: bool,
    /// The text of the most recent STR_I.
    text: String,
    /// The bytes of the most recent OPAQUE.
    opaque_bytes: Vec<u8>,
    /// The last event returned from `next()`.
    last_event: Option<DeserializerEvent>,
}

impl<'a> Deserializer<'a> {
    /// Construct a new deserializer over `input`. Reads the 4-byte header on
    /// construction; returns an error if the header is malformed or the input
    /// is empty.
    ///
    /// # Errors
    ///
    /// Returns `WbxmlError` when the 4-byte header cannot be read or is wrong
    /// (`EmptyStream`, `UnexpectedEof`, `InvalidMultibyteInteger`) or the charset
    /// is not UTF-8 (`InvalidUtf8`).
    pub fn new(input: &'a [u8]) -> WbxmlResult<Self> {
        let mut d = Self {
            input,
            pos: 0,
            open_stack: Vec::with_capacity(16),
            current_page: 0,
            current_tag: (0, 0),
            no_content: false,
            text: String::new(),
            opaque_bytes: Vec::new(),
            last_event: None,
        };
        d.read_header()?;
        Ok(d)
    }

    fn read_header(&mut self) -> WbxmlResult<()> {
        if self.input.is_empty() {
            return Err(WbxmlError::EmptyStream);
        }
        // Version byte — ArkTS doesn't validate it (only reads), and neither
        // do we, but it must be present.
        let _version = self.read_byte()?;
        // Public identifier (mb_u_int32, typically 0x01).
        let _public_id = self.read_uint()?;
        // Charset (mb_u_int32, must be 106 = UTF-8).
        let charset = self.read_uint()?;
        if charset != 0x6A {
            // ArkTS helper silently bails; the parser throws. We surface an error
            // since the codec is UTF-8-only.
            return Err(WbxmlError::InvalidUtf8);
        }
        let string_table_len = self.read_uint()?;
        if string_table_len != 0 {
            return Err(WbxmlError::StringTableUnsupported);
        }
        Ok(())
    }

    /// Return the current start tag as `(page, token)`.
    pub fn current_tag(&self) -> (u8, u8) {
        self.current_tag
    }

    /// Return the text of the most recent STR_I event.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return the bytes of the most recent OPAQUE event.
    pub fn opaque(&self) -> &[u8] {
        &self.opaque_bytes
    }

    /// Return the most recent event, or `None` before the first `next_event()` call.
    pub fn last_event(&self) -> Option<DeserializerEvent> {
        self.last_event
    }

    /// Return the current depth (number of open elements).
    ///
    /// # Errors
    ///
    /// Returns `WbxmlError` when the response tree is malformed — an unexpected
    /// root or child tag, non-UTF-8 content, or non-numeric text where a number is
    /// required.
    pub fn depth(&self) -> usize {
        self.open_stack.len()
    }

    /// Advance to the next event. Returns `Done` when the document is exhausted.
    ///
    /// # Errors
    ///
    /// Returns `WbxmlError` on malformed input: `UnexpectedEof`, `UnknownCodePage`,
    /// `UnsupportedGlobalToken`, `AttributesUnsupported`, `StringTableUnsupported`,
    /// `InvalidMultibyteInteger`, or `InvalidUtf8`.
    pub fn next_event(&mut self) -> WbxmlResult<DeserializerEvent> {
        // Clear scratch state.
        self.text.clear();
        self.opaque_bytes.clear();

        // If the previous tag was degenerated (no content), synthesize an END
        // event before reading the next byte. This matches the ArkTS behavior
        // where `getNext()` emits an END immediately after the start.
        if self.no_content {
            self.no_content = false;
            // Pop is done by the `pop()` call inside the previous Start —
            // but the ArkTS code has a bug where `no_content` synthetic END
            // does NOT pop. The stack is managed on Start, not END. Verify
            // against the source: yes, `push` happens on Start, `pop` on
            // real END token. Synthetic no_content END neither pushes nor pops.
            self.last_event = Some(DeserializerEvent::End);
            return Ok(DeserializerEvent::End);
        }

        if self.pos >= self.input.len() {
            self.last_event = Some(DeserializerEvent::Done);
            return Ok(DeserializerEvent::Done);
        }

        let mut id = self.read_byte()?;
        while id == SWITCH_PAGE {
            let new_page = self.read_byte()?;
            if !code_pages::is_valid_page(new_page) {
                return Err(WbxmlError::UnknownCodePage(new_page));
            }
            self.current_page = new_page;
            id = self.read_byte()?;
        }

        let event = match id {
            END => {
                self.pop_tag();
                DeserializerEvent::End
            }
            STR_I => {
                self.text = self.read_inline_string()?;
                DeserializerEvent::Text
            }
            OPAQUE => {
                let length = self.read_uint()?;
                self.read_opaque(length as usize)?;
                DeserializerEvent::Opaque
            }
            // The remaining global tokens are all unsupported.
            ENTITY | LITERAL => {
                return Err(WbxmlError::UnsupportedGlobalToken(id));
            }
            _ => {
                // Tag token — check high bits.
                if (id & WITH_ATTRIBUTES) != 0 {
                    return Err(WbxmlError::AttributesUnsupported(id));
                }
                let token = id & 0x3F;
                let has_content = (id & 0x40) != 0;
                self.push_tag(token);
                self.no_content = !has_content;
                if !has_content {
                    // Degenerated — we'll emit END on the next next() call,
                    // but we already pushed the tag. The ArkTS code keeps the
                    // tag on the stack across the synthetic END — see `getNext()`
                    // branch `if (this.noContent) { this.startTagArray.popFirst(); ... }`
                    // which DOES pop. So we pop now.
                    self.pop_tag();
                }
                DeserializerEvent::Start
            }
        };
        self.last_event = Some(event);
        Ok(event)
    }

    // ---- internal helpers ---------------------------------------------------

    fn push_tag(&mut self, token: u8) {
        let pair = (self.current_page, token);
        self.current_tag = pair;
        self.open_stack.push(pair);
    }

    fn pop_tag(&mut self) {
        if let Some(popped) = self.open_stack.pop() {
            self.current_tag = popped;
        }
    }

    fn read_byte(&mut self) -> WbxmlResult<u8> {
        if let Some(&b) = self.input.get(self.pos) {
            self.pos += 1;
            Ok(b)
        } else {
            Err(WbxmlError::UnexpectedEof)
        }
    }

    /// Decode an mb_u_int32 (big-endian, base-128 with continuation bit).
    /// Max 5 bytes per the spec.
    fn read_uint(&mut self) -> WbxmlResult<u32> {
        let mut result: u32 = 0;
        let mut count = 0usize;
        loop {
            if count >= 5 {
                return Err(WbxmlError::InvalidMultibyteInteger);
            }
            let b = self.read_byte()?;
            result = (result << 7) | u32::from(b & 0x7F);
            count += 1;
            if (b & 0x80) == 0 {
                break;
            }
        }
        Ok(result)
    }

    fn read_inline_string(&mut self) -> WbxmlResult<String> {
        let start = self.pos;
        while self.pos < self.input.len() {
            if self.input[self.pos] == 0 {
                let bytes = &self.input[start..self.pos];
                self.pos += 1; // consume the NUL
                return Ok(String::from_utf8_lossy(bytes).into_owned());
            }
            self.pos += 1;
        }
        Err(WbxmlError::UnexpectedEof)
    }

    fn read_opaque(&mut self, length: usize) -> WbxmlResult<()> {
        if self.pos + length > self.input.len() {
            return Err(WbxmlError::UnexpectedEof);
        }
        self.opaque_bytes
            .extend_from_slice(&self.input[self.pos..self.pos + length]);
        self.pos += length;
        Ok(())
    }
}

/// Deserialize a complete WBXML document into a tree.
///
/// The root element is returned unwrapped (i.e. the document's outermost
/// element becomes the returned `WbxmlElement`).
///
/// # Errors
///
/// Propagates `WbxmlError` from the event loop (see
/// [`Deserializer::next_event`]).
pub fn deserialize_to_tree(input: &[u8]) -> WbxmlResult<WbxmlElement> {
    let mut d = Deserializer::new(input)?;
    // Stack of elements whose children we're currently filling. When an
    // element is closed (`End`), it is popped and appended to its parent
    // (or becomes the root if the stack was empty at pop time).
    let mut stack: Vec<WbxmlElement> = Vec::with_capacity(16);
    let mut root: Option<WbxmlElement> = None;

    loop {
        match d.next_event()? {
            DeserializerEvent::Done => break,
            DeserializerEvent::Start => {
                let (page, token) = d.current_tag();
                stack.push(WbxmlElement::empty(page, token));
            }
            DeserializerEvent::Text => {
                if let Some(top) = stack.last_mut() {
                    top.value = WbxmlValue::Text(d.text().to_owned());
                }
            }
            DeserializerEvent::Opaque => {
                if let Some(top) = stack.last_mut() {
                    top.value = WbxmlValue::Opaque(d.opaque().to_vec());
                }
            }
            DeserializerEvent::End => {
                let el = stack.pop().expect("END without matching Start");
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(el);
                } else if root.is_none() {
                    root = Some(el);
                } else {
                    // Multiple roots — shouldn't happen for well-formed
                    // MS-ASWBXML; treat as parse error.
                    return Err(WbxmlError::UnexpectedToken {
                        expected: "single root element",
                        got: END,
                    });
                }
            }
        }
    }

    root.ok_or(WbxmlError::UnexpectedEndOfDocument)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::global_tokens::WITH_CONTENT;

    #[test]
    fn empty_input_errors() {
        assert!(matches!(
            deserialize_to_tree(&[]),
            Err(WbxmlError::EmptyStream)
        ));
    }

    #[test]
    fn minimal_document_parses() {
        // Header + single degenerated AirSync:SyncKey tag (no END byte needed
        // — the deserializer synthesizes the END when it sees no content bit).
        let bytes = [0x03, 0x01, 0x6A, 0x00, 0x0B];
        let root = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(root, WbxmlElement::empty(0, 0x0B));
    }

    #[test]
    fn text_value_parses() {
        // Header + AirSync:SyncKey { text "abc" } + END
        let bytes = [
            0x03,
            0x01,
            0x6A,
            0x00,
            0x0B | WITH_CONTENT,
            STR_I,
            b'a',
            b'b',
            b'c',
            0x00,
            END,
        ];
        let root = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(root, WbxmlElement::text(0, 0x0B, "abc"));
    }

    #[test]
    fn page_switch_parses() {
        // Header + AirSync:SyncKey (page 0, token 0x0B, degenerated)
        // + SWITCH_PAGE 0x01 + Contacts:Anniversary (page 1, token 0x05, degenerated)
        let bytes = [0x03, 0x01, 0x6A, 0x00, 0x0B, SWITCH_PAGE, 0x01, 0x05];
        let mut d = Deserializer::new(&bytes).unwrap();
        let ev1 = d.next_event().unwrap();
        assert_eq!(ev1, DeserializerEvent::Start);
        assert_eq!(d.current_tag(), (0, 0x0B));
        let ev2 = d.next_event().unwrap(); // synthetic END for no_content tag
        assert_eq!(ev2, DeserializerEvent::End);
        let ev3 = d.next_event().unwrap();
        assert_eq!(ev3, DeserializerEvent::Start);
        assert_eq!(d.current_tag(), (1, 0x05));
    }

    #[test]
    fn nested_elements_parse() {
        // AirSync:Sync { AirSync:SyncKey "abc" }
        // SYNC = page 0 token 0x05 (with content)
        // SYNCKEY = page 0 token 0x0B (with content) STR_I "abc" 0x00
        let bytes = [
            0x03,
            0x01,
            0x6A,
            0x00,
            0x05 | WITH_CONTENT,
            0x0B | WITH_CONTENT,
            STR_I,
            b'a',
            b'b',
            b'c',
            0x00,
            END,
            END,
        ];
        let root = deserialize_to_tree(&bytes).unwrap();
        let expected = WbxmlElement::container(0, 0x05, vec![WbxmlElement::text(0, 0x0B, "abc")]);
        assert_eq!(root, expected);
    }

    #[test]
    fn opaque_data_parses() {
        // AirSync:Provision:Data { opaque [0x01, 0x02, 0x03] }
        // page 0x0E token 0x05 = Provision
        // page 0x0E token 0x0A = Data
        let bytes = [
            0x03,
            0x01,
            0x6A,
            0x00,
            SWITCH_PAGE,
            0x0E,
            0x05 | WITH_CONTENT, // Provision
            0x0A | WITH_CONTENT, // Data
            OPAQUE,
            0x03, // length 3
            0x01,
            0x02,
            0x03,
            END,
            END,
        ];
        let root = deserialize_to_tree(&bytes).unwrap();
        let expected = WbxmlElement::container(
            0x0E,
            0x05,
            vec![WbxmlElement::opaque(0x0E, 0x0A, vec![0x01, 0x02, 0x03])],
        );
        assert_eq!(root, expected);
    }

    #[test]
    fn string_table_rejected() {
        // Header with string_table_length = 1
        let bytes = [0x03, 0x01, 0x6A, 0x01];
        assert!(matches!(
            deserialize_to_tree(&bytes),
            Err(WbxmlError::StringTableUnsupported)
        ));
    }

    #[test]
    fn attributes_rejected() {
        // Header + tag with WITH_ATTRIBUTES bit (0x80 | 0x0B)
        let bytes = [0x03, 0x01, 0x6A, 0x00, 0x80 | 0x0B];
        let res = deserialize_to_tree(&bytes);
        assert!(matches!(res, Err(WbxmlError::AttributesUnsupported(_))));
    }

    #[test]
    fn entity_token_rejected() {
        let bytes = [0x03, 0x01, 0x6A, 0x00, ENTITY];
        assert!(matches!(
            deserialize_to_tree(&bytes),
            Err(WbxmlError::UnsupportedGlobalToken(t)) if t == ENTITY
        ));
    }

    #[test]
    fn multibyte_length_128_parses() {
        // AirSync:Provision:Data with opaque length 128 encoded as 0x81 0x00
        let mut bytes = vec![
            0x03,
            0x01,
            0x6A,
            0x00,
            SWITCH_PAGE,
            0x0E,
            0x05 | WITH_CONTENT,
            0x0A | WITH_CONTENT,
            OPAQUE,
            0x81,
            0x00, // length 128
        ];
        bytes.extend(std::iter::repeat_n(0xAAu8, 128));
        bytes.push(END);
        bytes.push(END);
        let root = deserialize_to_tree(&bytes).unwrap();
        match &root.children[0].value {
            WbxmlValue::Opaque(b) => assert_eq!(b.len(), 128),
            other => panic!("expected Opaque, got {other:?}"),
        }
    }
}
