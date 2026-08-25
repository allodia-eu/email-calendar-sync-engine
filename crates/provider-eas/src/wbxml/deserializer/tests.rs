// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

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
