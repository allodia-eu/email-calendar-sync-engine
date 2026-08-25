// SPDX-License-Identifier: MPL-2.0
//! SendMail/SmartForward/SmartReply tests: request trees and status parsing.
use provider_eas::commands::{tests_common::*, *};

#[test]
fn send_mail_request_minimal() {
    let req = SendMailRequest {
        mime: b"From: a@b\r\nSubject: t\r\n\r\nbody\r\n".to_vec(),
        save_to_sent: true,
        client_id: None,
    };
    let tree = build_send_mail_request(&req);
    assert_eq!(tree.page, PAGE_COMPOSE);
    assert_eq!(tree.token, compose::SEND_MAIL);
    // 3 children: ClientId (synthesized — spec-required per MS-ASCMD
    // 2.2.3.28.1) + SaveInSentItems + Mime.
    assert_eq!(tree.children.len(), 3);
    assert_eq!(tree.children[0].token, compose::CLIENT_ID);
    // Then SaveInSentItems (0x08) and Mime (0x10) as OPAQUE. This is the
    // regression guard for the old bug where SaveInSent (0x08) was
    // correct but Mime (0x10) was emitted with the wrong token
    // (0x09 = ReplaceMime).
    assert_eq!(tree.children[1].token, compose::SAVE_IN_SENT_ITEMS);
    assert_eq!(tree.children[2].token, compose::MIME);
    match &tree.children[2].value {
        WbxmlValue::Opaque(_) => {}
        other => panic!("expected Opaque Mime, got {other:?}"),
    }
}

#[test]
fn send_mail_request_round_trips() {
    let req = SendMailRequest {
        mime: b"From: a@b\r\nTo: c@d\r\nSubject: round trip\r\n\r\nbody\r\n".to_vec(),
        save_to_sent: true,
        client_id: Some("SendMail-rt-1".into()),
    };
    let tree = build_send_mail_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Regression guard for the WBXML tag-constant bug + the OPAQUE `<Mime>`
/// requirement. The serialized bytes MUST:
///   1. Contain the raw RFC5322 bytes verbatim (inside the OPAQUE block), NOT base64-encoded — this
///      is the silent-corruption fix.
///   2. Use the correct ComposeMail tokens: 0x05 (SendMail root via page switch), 0x08
///      (SaveInSentItems), 0x11 (ClientId), 0x10 (Mime), and the OPAQUE marker 0xC3 immediately
///      preceding the length + bytes. The old code emitted 0x09 for Mime (== ReplaceMime) and read
///      Status as 0x18 (unregistered) — both broken.
///   3. NOT emit `<Mime>` as a STR_I (0x03 inline string token) — that was the previous shape
///      (`text(...)`), which Exchange misreads.
#[test]
fn send_mail_request_emits_opaque_mime_with_correct_tokens() {
    // WBXML header is 4 bytes: version(0x03) + publicid(0x01) +
    // charset(0x6A=UTF-8) + strtable(0x00). When searching for STR_I
    // (0x03) we must skip these, otherwise the version byte is a false
    // positive.
    const HEADER_LEN: usize = 4;
    let raw_mime = b"From: a@b\r\nTo: c@d\r\nSubject: golden\r\n\r\nbody bytes\r\n";
    let req = SendMailRequest {
        mime: raw_mime.to_vec(),
        save_to_sent: true,
        client_id: Some("SendMail-1234".into()),
    };
    let el = build_send_mail_request(&req);
    let wbxml = provider_eas::wbxml::serialize_tree(&el).expect("serialize_tree");

    // 1. Raw MIME bytes appear verbatim inside the OPAQUE block, NOT base64. base64 of `raw_mime`
    //    would not contain any of the literal "From:" substring — this substring presence proves
    //    the bytes are raw.
    assert!(
        wbxml.windows(raw_mime.len()).any(|w| w == raw_mime),
        "raw MIME bytes must appear verbatim in WBXML (OPAQUE), not base64-encoded"
    );

    // 2a. The OPAQUE marker 0xC3 is present and immediately precedes the
    //     length-prefixed raw bytes. We find the marker and confirm the
    //     raw bytes start shortly after the mb_u_int32 length encoding.
    let opaque_idx = wbxml
        .iter()
        .position(|&b| b == 0xC3)
        .expect("OPAQUE marker 0xC3 must be present");
    // After 0xC3 comes a mb_u_int32 length, then the bytes. For a short
    // (<128B) payload the length is a single byte equal to raw_mime.len().
    assert_eq!(
        wbxml[opaque_idx + 1] as usize,
        raw_mime.len(),
        "single-byte mb_u_int32 length must equal raw MIME length for short payloads"
    );
    assert_eq!(
        &wbxml[opaque_idx + 2..opaque_idx + 2 + raw_mime.len()],
        raw_mime as &[u8],
        "raw MIME bytes must immediately follow the OPAQUE marker + length"
    );

    // 2b. The ComposeMail tokens are present. The serializer ORs the
    //     WITH_CONTENT bit (0x40) into any tag that has a child (STR_I,
    //     OPAQUE, or nested element) — so ClientId (0x11) and Mime (0x10)
    //     appear as 0x51 / 0x50, while the empty SaveInSentItems (0x08)
    //     appears bare. Accept either form when checking presence.
    assert!(
        wbxml
            .windows(3)
            .any(|w| w[0] == 0x00 && w[1] == PAGE_COMPOSE && w[2] == (compose::SEND_MAIL | 0x40)),
        "expected SWITCH_PAGE(0x00 {:#04x}) followed by SendMail-with-content ({:#04x})",
        PAGE_COMPOSE,
        compose::SEND_MAIL | 0x40,
    );
    // ClientId (0x11 / 0x51) — present because we set client_id=Some.
    assert!(
        wbxml.contains(&compose::CLIENT_ID) || wbxml.contains(&(compose::CLIENT_ID | 0x40)),
        "ClientId token 0x11/0x51 missing"
    );
    // SaveInSentItems (0x08) — present because save_to_sent=true. Empty
    // tags are emitted bare (no WITH_CONTENT bit).
    assert!(
        wbxml.contains(&compose::SAVE_IN_SENT_ITEMS),
        "SaveInSentItems token 0x08 missing"
    );
    // Mime (0x10 / 0x50) — the corrected token. The old buggy value was
    // 0x09 (== ReplaceMime); this assertion locks the fix.
    assert!(
        wbxml.contains(&compose::MIME) || wbxml.contains(&(compose::MIME | 0x40)),
        "Mime token 0x10/0x50 missing (regression: was 0x09 in the buggy build)"
    );

    // 3. The byte immediately before the OPAQUE marker must be the `<Mime>` tag token (0x10 |
    //    WITH_CONTENT 0x40 = 0x50), proving `<Mime>` is the element carrying the opaque payload. A
    //    STR_I `<Mime>` would instead show STR_I (0x03) here with no OPAQUE.
    assert_eq!(
        wbxml[opaque_idx - 1],
        compose::MIME | 0x40,
        "byte before OPAQUE must be <Mime> token ({:#04x}), got {:#04x} — \
         STR_I <Mime> regression?",
        compose::MIME | 0x40,
        wbxml[opaque_idx - 1],
    );
    // Sanity: the STR_I inline-string token (0x03) is NOT used to carry
    // the MIME payload. Search only in the body (after the 4-byte WBXML
    // header) so the version byte (which happens to be 0x03) is excluded.
    // The only STR_I in a correct request is the ClientId value, which
    // holds the literal "SendMail-1234" — NOT a base64/inline MIME.
    let body = &wbxml[HEADER_LEN..];
    let str_i_idx = body.iter().position(|&b| b == 0x03);
    if let Some(i) = str_i_idx {
        let cid = b"SendMail-1234";
        assert_eq!(
            &body[i + 1..i + 1 + cid.len()],
            cid as &[u8],
            "the only STR_I in the body must be the ClientId, not an inline <Mime>"
        );
    }
}

#[test]
fn smart_forward_request_round_trips() {
    let req = SmartForwardRequest {
        mime_base64: "U0VG".to_string(),
        source_server_id: "srv-1".to_string(),
        source_collection_id: "col-1".to_string(),
        save_to_sent: false,
        replace_mime: true,
        client_id: None,
    };
    let tree = build_smart_forward_request(&req).expect("build");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

#[test]
fn smart_reply_request_round_trips() {
    let req = SmartReplyRequest {
        mime_base64: "U0VG".to_string(),
        source_server_id: "srv-1".to_string(),
        source_collection_id: "col-1".to_string(),
        save_to_sent: true,
        replace_mime: false,
        client_id: None,
    };
    let tree = build_smart_reply_request(&req).expect("build");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

// ---- F10-3: SmartForward/SmartReply schema fixes ----
//
// [MS-ASCMD] 6.41/6.43 request schemas document the element order
// ClientId, Source, SaveInSentItems, ReplaceMime, Mime, and Exchange 15.2
// enforces it: the pre-fix builders emitted SaveInSentItems + Mime BEFORE
// Source with no ClientId and were rejected with in-body Status 103
// (live evidence 2026-08-02). The same probe run verified the spec shape
// (ClientId, Source(FolderId, ItemId), raw-MIME opaque) returns HTTP 200
// empty body = success.

/// Spec order with ClientId, save_to_sent=false, replace_mime=false:
/// exactly [ClientId, Source(FolderId, ItemId), Mime] — no SaveInSentItems,
/// no ReplaceMime.
#[test]
fn smart_forward_request_emits_spec_order() {
    let req = SmartForwardRequest {
        mime_base64: base64_encode(b"From: a@b\r\n\r\nbody\r\n"),
        source_server_id: "srv-9".to_string(),
        source_collection_id: "col-7".to_string(),
        save_to_sent: false,
        replace_mime: false,
        client_id: Some("SF-1".to_string()),
    };
    let tree = build_smart_forward_request(&req).expect("build");
    assert_eq!(
        (tree.page, tree.token),
        (PAGE_COMPOSE, compose::SMART_FORWARD)
    );
    let tokens: Vec<u8> = tree.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![compose::CLIENT_ID, compose::SOURCE, compose::MIME],
        "spec order: ClientId, Source, Mime (no SaveInSentItems / ReplaceMime when gated off)"
    );
    // ClientId text value.
    assert_eq!(text_value(&tree.children[0]).unwrap(), "SF-1");
    // Source children: FolderId THEN ItemId (spec sequence).
    let source = &tree.children[1];
    assert_eq!(
        source.children.iter().map(|c| c.token).collect::<Vec<_>>(),
        vec![compose::FOLDER_ID, compose::ITEM_ID]
    );
    assert_eq!(text_value(&source.children[0]).unwrap(), "col-7");
    assert_eq!(text_value(&source.children[1]).unwrap(), "srv-9");
}

/// The Mime payload must be the RAW MIME entity as OPAQUE — the DTO's
/// base64 string decoded — NOT the base64 text bytes.
#[test]
fn smart_forward_request_emits_raw_mime_not_base64() {
    let raw = b"From: a@b\r\nTo: c@d\r\nSubject: fwd\r\n\r\nforwarded body\r\n".to_vec();
    let req = SmartForwardRequest {
        mime_base64: base64_encode(&raw),
        source_server_id: "srv-1".to_string(),
        source_collection_id: "col-1".to_string(),
        save_to_sent: false,
        replace_mime: false,
        client_id: None,
    };
    let tree = build_smart_forward_request(&req).expect("build");
    let mime_el = tree
        .children
        .iter()
        .find(|c| c.token == compose::MIME)
        .expect("Mime element");
    match &mime_el.value {
        WbxmlValue::Opaque(bytes) => assert_eq!(
            bytes, &raw,
            "Mime must carry the decoded raw MIME entity, not the base64 text"
        ),
        other => panic!("expected Opaque Mime, got {other:?}"),
    }
}

/// SaveInSentItems is gated on save_to_sent and ReplaceMime on
/// replace_mime; when both are on they sit between Source and Mime.
#[test]
fn smart_forward_request_gates_save_and_replace() {
    let req = SmartForwardRequest {
        mime_base64: base64_encode(b"X"),
        source_server_id: "srv-1".to_string(),
        source_collection_id: "col-1".to_string(),
        save_to_sent: true,
        replace_mime: true,
        client_id: None,
    };
    let tree = build_smart_forward_request(&req).expect("build");
    let tokens: Vec<u8> = tree.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![
            compose::CLIENT_ID, // synthesized when caller passes None (spec-required)
            compose::SOURCE,
            compose::SAVE_IN_SENT_ITEMS,
            compose::REPLACE_MIME,
            compose::MIME
        ],
        "spec order: ClientId, Source, SaveInSentItems, ReplaceMime, Mime"
    );
}

/// An invalid base64 payload is a client-side error surfaced BEFORE any
/// bytes hit the wire — never sent to the server.
#[test]
fn smart_forward_request_rejects_invalid_base64() {
    let req = SmartForwardRequest {
        mime_base64: "!!!not-base64!!!".to_string(),
        source_server_id: "srv-1".to_string(),
        source_collection_id: "col-1".to_string(),
        save_to_sent: false,
        replace_mime: false,
        client_id: None,
    };
    assert!(build_smart_forward_request(&req).is_err());
    let reply = SmartReplyRequest {
        mime_base64: "!!!not-base64!!!".to_string(),
        source_server_id: "srv-1".to_string(),
        source_collection_id: "col-1".to_string(),
        save_to_sent: false,
        replace_mime: false,
        client_id: None,
    };
    assert!(build_smart_reply_request(&reply).is_err());
}

/// SmartReply shares the SmartForward child sequence (same [MS-ASCMD]
/// schema shape, 6.43) — same order, same raw-MIME rule.
#[test]
fn smart_reply_request_emits_spec_order_and_raw_mime() {
    let raw = b"From: a@b\r\n\r\nreply body\r\n".to_vec();
    let req = SmartReplyRequest {
        mime_base64: base64_encode(&raw),
        source_server_id: "srv-2".to_string(),
        source_collection_id: "col-3".to_string(),
        save_to_sent: false,
        replace_mime: false,
        client_id: Some("SR-1".to_string()),
    };
    let tree = build_smart_reply_request(&req).expect("build");
    assert_eq!(
        (tree.page, tree.token),
        (PAGE_COMPOSE, compose::SMART_REPLY)
    );
    let tokens: Vec<u8> = tree.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![compose::CLIENT_ID, compose::SOURCE, compose::MIME]
    );
    match &tree.children[2].value {
        WbxmlValue::Opaque(bytes) => assert_eq!(bytes, &raw),
        other => panic!("expected Opaque Mime, got {other:?}"),
    }
}
