// SPDX-License-Identifier: MPL-2.0
// Multipart ItemOperations response support ([MS-ASCMD] §2.2.1.10.1,
// §4.10.5). A server that sees the `MS-ASAcceptMultiPart: T` request header
// MAY answer ItemOperations with Content-Type
// `application/vnd.ms-sync.multipart`: a MultiPartResponse binary envelope
// whose first part is the WBXML tree and whose later parts carry the large
// binary payloads (bodies, attachments) OUT of the WBXML stream. Inside the
// WBXML tree an `itemoperations:Part` element (ItemOperations page 20,
// token 0x11 — [MS-ASCMD] §2.2.3.130) replaces the usual
// `airsyncbase:Data` child and holds the integer index of the part carrying
// the payload.

use crate::{
    client::EasError,
    wbxml::{
        tags::{self, pages},
        types::{WbxmlElement, WbxmlValue},
    },
};

/// ItemOperations WBXML code page ([MS-ASWBXML] §2.1.2.1.21). `tags::pages`
/// stops at FIND (0x19); ItemOperations is page 20.
const PAGE_ITEM_OPS: u8 = 20;

/// One PartMetaData entry ([MS-ASCMD] §2.2.1.10.1.1.1): where a part's
/// bytes live inside the MultiPartResponse buffer. Offsets are from the
/// START of the whole structure (header included), not from the Parts
/// field — the §4.10.5.2 example confirms (part 0 at offset 20 = 4-byte
/// PartCount + two 8-byte PartMetaData entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartMetaData {
    /// Byte offset of the part, from the START of the whole structure.
    pub offset: u32,
    /// Part length in bytes.
    pub length: u32,
}

/// Parsed MultiPartResponse: the metadata table plus each part's bytes
/// sliced out of the buffer. `parts[i]` corresponds to `metadata[i]`.
#[derive(Debug, Clone)]
pub struct MultipartParts {
    /// Per-part offset/length table, part order.
    pub metadata: Vec<PartMetaData>,
    /// Part bytes sliced out of the buffer, part order.
    pub parts: Vec<Vec<u8>>,
}

/// Parse a MultiPartResponse body ([MS-ASCMD] §2.2.1.10.1.1):
///
/// ```text
/// PartCount          u32 little-endian
/// PartsMetaData      PartCount × (Offset u32 LE, Length u32 LE)
/// Parts              raw bytes; part i = buffer[meta[i].offset .. +length]
/// ```
///
/// Every bound is checked with u64 arithmetic so a malicious or corrupt
/// buffer (huge PartCount, wrapping offset+length, ranges overlapping the
/// header/metadata) yields a descriptive error, never a panic or a wrap.
///
/// # Errors
///
/// Returns `EasError::Transport` describing the inconsistency for a truncated
/// or corrupt buffer (bad PartCount, out-of-bounds or header-overlapping part
/// ranges) — every bound is checked, so a hostile body can never panic.
pub fn parse_multipart_response(bytes: &[u8]) -> Result<MultipartParts, EasError> {
    if bytes.len() < 4 {
        return Err(EasError::Transport(format!(
            "multipart response truncated: PartCount header needs 4 bytes, got {}",
            bytes.len()
        )));
    }
    let count = u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
    let metadata_end = 4u64
        .checked_add(count.checked_mul(8).ok_or_else(|| {
            EasError::Transport(format!(
                "multipart response PartCount {count} overflows the metadata table size"
            ))
        })?)
        .ok_or_else(|| {
            EasError::Transport(format!(
                "multipart response PartCount {count} overflows the metadata table size"
            ))
        })?;
    if metadata_end > bytes.len() as u64 {
        return Err(EasError::Transport(format!(
            "multipart response truncated: PartCount {count} needs {metadata_end} bytes of header+metadata, buffer has {}",
            bytes.len()
        )));
    }
    // Both are bounded by bytes.len() by the check above, so both fit
    // usize on every target width.
    let metadata_end = usize::try_from(metadata_end).expect("checked ≤ bytes.len()");
    let count = usize::try_from(count).expect("count < metadata_end ≤ bytes.len()");
    let mut metadata = Vec::with_capacity(count);
    let mut parts = Vec::with_capacity(count);
    for i in 0..count {
        let base = 4 + i * 8;
        let offset = u32::from_le_bytes([
            bytes[base],
            bytes[base + 1],
            bytes[base + 2],
            bytes[base + 3],
        ]);
        let length = u32::from_le_bytes([
            bytes[base + 4],
            bytes[base + 5],
            bytes[base + 6],
            bytes[base + 7],
        ]);
        let start = u64::from(offset);
        let end = start + u64::from(length);
        if end > bytes.len() as u64 {
            return Err(EasError::Transport(format!(
                "multipart part {i} out of bounds: offset {offset} + length {length} = {end} exceeds buffer of {} bytes",
                bytes.len()
            )));
        }
        if usize::try_from(start).expect("start < end ≤ bytes.len() (checked above)") < metadata_end
        {
            return Err(EasError::Transport(format!(
                "multipart part {i} overlaps the header/metadata: offset {offset} is inside the first {metadata_end} bytes"
            )));
        }
        metadata.push(PartMetaData { offset, length });
        let start = usize::try_from(start).expect("start < end ≤ bytes.len() (checked above)");
        let end = usize::try_from(end).expect("end ≤ bytes.len() (checked above)");
        parts.push(bytes[start..end].to_vec());
    }
    Ok(MultipartParts { metadata, parts })
}

/// Resolve `itemoperations:Part` elements ([MS-ASCMD] §2.2.3.130) inside
/// `airsyncbase:Body` containers against the parsed parts. Each Part
/// element is replaced by an `airsyncbase:Data` child carrying the
/// referenced part's bytes as base64 TEXT — matching the existing inline
/// convention (`parse_item_operations_response` reads Data as text base64
/// or opaque bytes and surfaces a base64 string either way), so everything
/// downstream (`ItemOperationsFetchResult.data`) is unchanged.
///
/// Bodies without a Part child pass through untouched. A non-numeric index
/// or an index beyond the parts vector is a descriptive error — a server
/// that sent us a multipart envelope we cannot reconcile must fail loudly,
/// not silently drop the body.
///
/// # Errors
///
/// Returns `EasError::Transport` when a `Part` index is non-numeric or beyond
/// the parsed parts vector — an unreconcilable envelope fails loudly rather
/// than silently dropping the body.
pub fn resolve_part_elements(root: &mut WbxmlElement, parts: &[Vec<u8>]) -> Result<(), EasError> {
    if root.page == pages::BASE
        && root.token == tags::base::BODY
        && let Some(pos) = root
            .children
            .iter()
            .position(|c| c.page == PAGE_ITEM_OPS && c.token == tags::item_operations::PART)
    {
        let part_el = root.children.remove(pos);
        let index_text = match &part_el.value {
            WbxmlValue::Text(t) => t.trim().to_string(),
            other => {
                return Err(EasError::Transport(format!(
                    "multipart itemoperations:Part must be integer text, got {other:?}"
                )));
            }
        };
        let index: usize = index_text.parse().map_err(|_| {
            EasError::Transport(format!(
                "multipart itemoperations:Part value '{index_text}' is not a valid part index"
            ))
        })?;
        let part_bytes = parts.get(index).ok_or_else(|| {
                EasError::Transport(format!(
                    "multipart itemoperations:Part references part {index} but the response carries {} part(s)",
                    parts.len()
                ))
            })?;
        root.children.push(WbxmlElement::text(
            pages::BASE,
            tags::base::DATA,
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, part_bytes),
        ));
    }
    for child in &mut root.children {
        resolve_part_elements(child, parts)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::{
        deserialize_to_tree,
        tags::{self, pages},
        types::{WbxmlElement, WbxmlValue},
    };

    /// The COMPLETE binary response body from [MS-ASCMD] §4.10.5.2 (the spec
    /// prints it as a wrapped hex dump; this is the exact byte sequence,
    /// verified against the spec's own interpretation: PartCount=2,
    /// PartsMetaData[0]={offset 20, length 500},
    /// PartsMetaData[1]={offset 520, length 19}). One same-length
    /// substitution from the spec bytes: the To/Cc display addresses use a
    /// reserved example.net mailbox instead of the spec's contoso.com one —
    /// contoso.com is a real registered domain, and fixture identifiers
    /// stay reserved (RFC 2606); the lengths match so every recorded part
    /// offset/length above still holds.
    const SPEC_HEX: &str = concat!(
        "0200000014000000F4010000080200001300000003016A000014454D03310001",
        "4E464D03310001000052033500014D03353A3100015003456D61696C00010014",
        "4B0002560322446576696365205573657222203C646576696365757365724065",
        "78616D706C652E6E65743E0001580322446576696365205573657222203C6465",
        "7669636575736572406578616D706C652E6E65743E0001540354657374204D75",
        "6C74697061727420526573706F6E736500014F03323031322D30352D30385432",
        "303A31353A34352E3231315A0001510344657669636520557365720001750354",
        "657374204D756C74697061727420526573706F6E736500015203310001550330",
        "000100114E4F5003746573742E7478740001510352674141414143333064496F",
        "6B2532624A6854616962504766417173414B427744326C516E4B253266626241",
        "5149526455774230396B253262484141414141414156414144326C516E4B2532",
        "666262415149526455774230396B25326248414146347141253262534141414A",
        "25336130000152033100014C033733000101014A46033100014C033139000100",
        "145103310001010002530349504D2E4E6F746500017903323835393100013A7C",
        "0375726E3A636F6E74656E742D636C61737365733A6D65737361676500010011",
        "5603320001001649C310ADA57BDA90934B4CAE83B1A1AFDDEAD5014AC305CD2D",
        "5758360101010101546869732069732074686520626F64792E0D0A"
    );

    fn spec_blob() -> Vec<u8> {
        hex::decode(SPEC_HEX).expect("embedded spec hex must decode")
    }

    /// Find the first airsyncbase:Body container in a tree (recursive).
    fn find_body(el: &WbxmlElement) -> Option<&WbxmlElement> {
        if el.page == pages::BASE && el.token == tags::base::BODY {
            return Some(el);
        }
        el.children.iter().find_map(find_body)
    }

    #[test]
    fn spec_example_parses_into_two_parts_with_spec_metadata() {
        let parsed = parse_multipart_response(&spec_blob()).expect("spec blob must parse");
        assert_eq!(parsed.parts.len(), 2, "PartCount field says 2");
        assert_eq!(parsed.metadata.len(), 2);
        // Spec interpretation (§4.10.5.2): Offset 20, Length 500.
        assert_eq!(parsed.metadata[0].offset, 20);
        assert_eq!(parsed.metadata[0].length, 500);
        // Spec interpretation: Offset 520, Length 19.
        assert_eq!(parsed.metadata[1].offset, 520);
        assert_eq!(parsed.metadata[1].length, 19);
    }

    #[test]
    fn spec_example_part_zero_decodes_as_wbxml() {
        let parsed = parse_multipart_response(&spec_blob()).expect("parse");
        assert_eq!(parsed.parts[0].len(), 500);
        let tree = deserialize_to_tree(&parsed.parts[0])
            .expect("part 0 must round-trip through the WBXML deserializer");
        // ItemOperations root: page 20, token 0x05.
        assert_eq!((tree.page, tree.token), (20, 0x05));
        // The spec XML shows airsyncbase:Body carrying <Part>1</Part>.
        let body = find_body(&tree).expect("spec tree has a Body element");
        let part = body
            .children
            .iter()
            .find(|c| c.page == 20 && c.token == tags::item_operations::PART)
            .expect("Body must carry an itemoperations:Part child");
        assert_eq!(part.value, WbxmlValue::Text("1".to_string()));
    }

    #[test]
    fn spec_example_part_one_is_the_body_text() {
        let parsed = parse_multipart_response(&spec_blob()).expect("parse");
        assert_eq!(parsed.parts[1], b"This is the body.\r\n".to_vec());
    }

    // ---- malformed inputs: descriptive errors, never panics ----

    #[test]
    fn truncated_header_errors() {
        for blob in [&b""[..], &b"\x02"[..], &b"\x02\x00\x00"[..]] {
            let err =
                parse_multipart_response(blob).expect_err("fewer than 4 header bytes must error");
            let msg = err.to_string();
            assert!(
                msg.contains("truncated"),
                "error must say the header is truncated, got: {msg}"
            );
        }
    }

    #[test]
    fn huge_part_count_overrunning_buffer_errors() {
        // PartCount 0x00010000 = 65536 metadata entries → 524288 bytes of
        // metadata against a 4-byte buffer.
        let blob = 0x0001_0000u32.to_le_bytes();
        let err = parse_multipart_response(&blob).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("65536") && msg.contains("metadata"),
            "error must name the part count and the metadata overrun, got: {msg}"
        );
    }

    #[test]
    fn part_range_out_of_bounds_errors() {
        // count=1, metadata present, but offset+length runs past the buffer.
        let mut blob = Vec::new();
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&12u32.to_le_bytes()); // offset 12 (right after metadata)
        blob.extend_from_slice(&500u32.to_le_bytes()); // length 500 — buffer is 12 bytes
        let err = parse_multipart_response(&blob).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("out of bounds"),
            "error must describe the bounds violation, got: {msg}"
        );
    }

    #[test]
    fn part_overlapping_metadata_errors() {
        // count=2 → parts region starts at 4 + 2*8 = 20. A part claiming
        // offset 0 overlaps the header/metadata.
        let mut blob = vec![0u8; 32];
        blob[..4].copy_from_slice(&2u32.to_le_bytes());
        blob[4..8].copy_from_slice(&0u32.to_le_bytes()); // offset 0 — overlaps header
        blob[8..12].copy_from_slice(&4u32.to_le_bytes()); // length 4
        blob[12..16].copy_from_slice(&20u32.to_le_bytes()); // part 1: offset 20, fine
        blob[16..20].copy_from_slice(&4u32.to_le_bytes());
        let err = parse_multipart_response(&blob).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("overlap"),
            "error must call out the metadata overlap, got: {msg}"
        );
    }

    #[test]
    fn offset_plus_length_overflow_errors_without_panic() {
        // offset = u32::MAX, length = u32::MAX: a naive u32/usize add wraps.
        let mut blob = Vec::new();
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&u32::MAX.to_le_bytes());
        blob.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = parse_multipart_response(&blob).expect_err("must error, not wrap");
        assert!(err.to_string().contains("out of bounds"));
    }

    // ---- Part-element resolution ----

    fn synthetic_fetch_tree(part_index_text: &str) -> WbxmlElement {
        // ItemOperations > Response > Fetch > Properties > airsyncbase:Body
        //   > [Type 1, itemoperations:Part {part_index_text}]
        WbxmlElement::container(
            20,
            tags::item_operations::ITEM_OPERATIONS,
            vec![WbxmlElement::container(
                20,
                tags::item_operations::RESPONSE,
                vec![WbxmlElement::container(
                    20,
                    tags::item_operations::FETCH,
                    vec![WbxmlElement::container(
                        20,
                        tags::item_operations::PROPERTIES,
                        vec![WbxmlElement::container(
                            pages::BASE,
                            tags::base::BODY,
                            vec![
                                WbxmlElement::text(pages::BASE, tags::base::TYPE, "1"),
                                WbxmlElement::text(
                                    20,
                                    tags::item_operations::PART,
                                    part_index_text,
                                ),
                            ],
                        )],
                    )],
                )],
            )],
        )
    }

    #[test]
    fn resolve_replaces_part_with_base64_data_child() {
        let mut root = synthetic_fetch_tree("1");
        let parts = vec![b"<wbxml>".to_vec(), b"hello body".to_vec()];
        resolve_part_elements(&mut root, &parts).expect("resolve must succeed");
        let body = find_body(&root).expect("body still present");
        assert!(
            body.children
                .iter()
                .all(|c| !(c.page == 20 && c.token == tags::item_operations::PART)),
            "the Part element must be gone after resolution"
        );
        let data = body
            .children
            .iter()
            .find(|c| c.page == pages::BASE && c.token == tags::base::DATA)
            .expect("Body must gain a Data child");
        // base64("hello body") — the existing Data convention for binary
        // payloads (matches parse_item_operations_response's Opaque arm).
        assert_eq!(data.value, WbxmlValue::Text("aGVsbG8gYm9keQ==".to_string()));
    }

    #[test]
    fn resolve_part_index_out_of_range_errors() {
        let mut root = synthetic_fetch_tree("5");
        let parts = vec![b"<wbxml>".to_vec(), b"body".to_vec()];
        let err = resolve_part_elements(&mut root, &parts).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains('5') && msg.contains('2'),
            "error must name the bad index and the part count, got: {msg}"
        );
    }

    #[test]
    fn resolve_part_non_numeric_errors() {
        let mut root = synthetic_fetch_tree("abc");
        let parts = vec![b"<wbxml>".to_vec()];
        let err = resolve_part_elements(&mut root, &parts).expect_err("must error");
        assert!(err.to_string().contains("abc"));
    }

    #[test]
    fn resolve_body_without_part_is_untouched() {
        let data_child = WbxmlElement::text(pages::BASE, tags::base::DATA, "aGVsbG8=");
        let mut root = WbxmlElement::container(
            20,
            tags::item_operations::ITEM_OPERATIONS,
            vec![WbxmlElement::container(
                pages::BASE,
                tags::base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, tags::base::TYPE, "2"),
                    data_child,
                ],
            )],
        );
        let parts = vec![b"<wbxml>".to_vec()];
        resolve_part_elements(&mut root, &parts).expect("no Part → no-op");
        let body = find_body(&root).unwrap();
        assert_eq!(body.children.len(), 2, "children unchanged");
    }

    /// End-to-end at the parser level: the spec blob's WBXML part, resolved
    /// against the parts vector, must feed `parse_item_operations_response`
    /// exactly as if the server had inlined the body as base64 Data.
    #[test]
    fn spec_example_resolved_tree_feeds_item_operations_parser() {
        let parsed = parse_multipart_response(&spec_blob()).expect("parse");
        let mut tree = deserialize_to_tree(&parsed.parts[0]).expect("wbxml");
        resolve_part_elements(&mut tree, &parsed.parts).expect("resolve");
        let result =
            crate::commands::parse_item_operations_response(&tree).expect("item operations parse");
        assert_eq!(result.status, 1);
        // base64("This is the body.\r\n")
        assert_eq!(result.data.as_deref(), Some("VGhpcyBpcyB0aGUgYm9keS4NCg=="));
        // Body Type 1 → text/plain fallback when the server sent no
        // airsyncbase:ContentType (existing parser convention).
        assert_eq!(result.content_type.as_deref(), Some("text/plain"));
    }
}
