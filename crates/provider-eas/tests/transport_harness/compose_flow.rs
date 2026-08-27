// SPDX-License-Identifier: MPL-2.0
//! Compose scenarios ([MS-ASCMD] SendMail family): the empty-body success
//! contract, the OPAQUE `<Mime>` wire shape, in-body rejection parsing, and
//! the SmartForward → SendMail degradation on both rejection shapes.

use std::sync::Arc;

use provider_eas::{
    types::{SendMailRequest, SmartForwardRequest, SmartReplyRequest},
    wbxml::{
        WbxmlElement, WbxmlValue,
        tags::{compose, pages},
    },
};

use super::{
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

const PAGE_COMPOSE_ROOT_SEND: u8 = 0x05;
const PAGE_COMPOSE_ROOT_FORWARD: u8 = 0x06;

/// SendMail success is an HTTP 200 with an EMPTY body (MS-ASCMD §2.2.1.13);
/// the request carries the raw RFC 5322 bytes as an OPAQUE `<Mime>` element.
#[tokio::test]
async fn send_mail_succeeds_with_an_empty_body_and_opaque_mime() {
    super::harness::init_logger();
    let mime: &[u8] =
        b"From: user@example.test\r\nTo: alice@example.net\r\nSubject: Hi\r\n\r\nbody\r\n";
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let mut client = client_at(&server.eas_url());
    let status = client
        .send_mail(&SendMailRequest {
            mime: mime.to_vec(),
            save_to_sent: true,
            client_id: Some("SendMail-test-1".into()),
        })
        .await
        .expect("empty body is success");
    assert_eq!(status, 1, "the empty-body contract reads as status 1");

    // The request's <Mime> carried the raw bytes as OPAQUE (page 21, 0x10) —
    // not inline text, which corrupts binary MIME.
    let tree = server.request(1).wbxml_tree().expect("request decodes");
    let mime_el = find_token(&tree, pages::COMPOSE, compose::MIME).expect("Mime element");
    assert!(
        matches!(&mime_el.value, WbxmlValue::Opaque(b) if b == mime),
        "Mime must be OPAQUE and byte-exact"
    );
}

/// An HTTP 200 with an in-body `<Status>` IS the failure shape — the client
/// surfaces the status value.
#[tokio::test]
async fn send_mail_in_body_status_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&compose_status_response(PAGE_COMPOSE_ROOT_SEND, "132"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let status = client
        .send_mail(&SendMailRequest {
            mime: b"Subject: x\r\n\r\nx".to_vec(),
            save_to_sent: true,
            client_id: Some("SendMail-test-2".into()),
        })
        .await
        .expect("in-body status parses");
    assert_eq!(status, 132);
}

/// SmartForward rejected in-body (Status 3, e.g. source item gone) degrades
/// to a plain SendMail carrying the same MIME — two requests, the second
/// with Cmd=SendMail.
#[tokio::test]
async fn smart_forward_in_body_rejection_degrades_to_send_mail() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::wbxml(&compose_status_response(PAGE_COMPOSE_ROOT_FORWARD, "3"))
        } else {
            MockResponse::empty_wbxml()
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let status = client
        .smart_forward(&SmartForwardRequest {
            mime_base64: "U3ViamVjdDogZm9yd2FyZGVk".into(),
            source_server_id: "srv:9".into(),
            source_collection_id: "fid-inbox".into(),
            save_to_sent: true,
            replace_mime: false,
            client_id: Some("SFWD-test-1".into()),
        })
        .await
        .expect("degradation rescues the send");
    assert_eq!(status, 1);
    let order: Vec<_> = server.captured().iter().map(CapturedRequest::cmd).collect();
    assert_eq!(
        order,
        vec![Some("SmartForward".into()), Some("SendMail".into())],
        "the fallback must re-send as plain SendMail"
    );
}

/// SmartForward rejected at the transport level (the MS-ASProtocolStatus
/// header path → `CommandStatus`) also degrades to SendMail.
#[tokio::test]
async fn smart_forward_command_status_rejection_degrades_to_send_mail() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::empty_wbxml().with_header("MS-ASProtocolStatus", "110")
        } else {
            MockResponse::empty_wbxml()
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let status = client
        .smart_forward(&SmartForwardRequest {
            mime_base64: "U3ViamVjdDogZg==".into(),
            source_server_id: "srv:9".into(),
            source_collection_id: "fid-inbox".into(),
            save_to_sent: false,
            replace_mime: false,
            client_id: None,
        })
        .await
        .expect("header rejection also degrades");
    assert_eq!(status, 1);
    assert_eq!(server.count(), 2);
    // With no client_id on the request, the degradation synthesizes one
    // (Exchange 15.2 rejects ClientId-less compose with status 103).
    let fallback_tree = server.request(2).wbxml_tree().expect("fallback decodes");
    assert!(
        find_token(&fallback_tree, pages::COMPOSE, compose::CLIENT_ID).is_some(),
        "the degraded SendMail must carry a (synthesized) ClientId"
    );
}

/// A TRANSPORT error (not a command rejection) does NOT degrade — surfacing
/// unchanged is correct, the SmartForward may succeed on retry.
#[tokio::test]
async fn smart_forward_transport_error_does_not_degrade() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::raw(503, "text/plain", "later")
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .smart_forward(&SmartForwardRequest {
            mime_base64: "Ug==".into(),
            source_server_id: "srv:9".into(),
            source_collection_id: "fid-inbox".into(),
            save_to_sent: true,
            replace_mime: false,
            client_id: Some("SFWD-test-3".into()),
        })
        .await
        .expect_err("503 surfaces");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::HttpStatus { status: 503, .. }
        ),
        "expected HttpStatus 503, got {err:?}"
    );
    assert_eq!(
        server.count(),
        1,
        "no SendMail fallback after a transport error"
    );
}

/// SmartReply success: empty body → status 1; the request names the source.
#[tokio::test]
async fn smart_reply_succeeds_and_names_the_source() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let mut client = client_at(&server.eas_url());
    let status = client
        .smart_reply(&SmartReplyRequest {
            mime_base64: "U3ViamVjdDogcg==".into(),
            source_server_id: "srv:12".into(),
            source_collection_id: "fid-inbox".into(),
            save_to_sent: true,
            replace_mime: false,
            client_id: Some("SREP-test-1".into()),
        })
        .await
        .expect("empty body is success");
    assert_eq!(status, 1);
    let tree = server.request(1).wbxml_tree().expect("request decodes");
    let source = find_token(&tree, pages::COMPOSE, compose::SOURCE).expect("Source element");
    let item_id = source
        .children
        .iter()
        .find(|c| c.token == compose::ITEM_ID)
        .expect("ItemId child");
    assert!(
        matches!(&item_id.value, WbxmlValue::Text(t) if t == "srv:12"),
        "the reply names the source ServerId"
    );
}

/// A corrupt `mime_base64` on the degradation path is a Transport error
/// (the fallback cannot decode what it must re-send).
#[tokio::test]
async fn smart_forward_garbage_mime_fails_the_degrade_decode() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&compose_status_response(PAGE_COMPOSE_ROOT_FORWARD, "3"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .smart_forward(&SmartForwardRequest {
            mime_base64: "!!!not-base64!!!".into(),
            source_server_id: "srv:9".into(),
            source_collection_id: "fid-inbox".into(),
            save_to_sent: true,
            replace_mime: false,
            client_id: Some("SFWD-test-4".into()),
        })
        .await
        .expect_err("undecodable MIME must fail");
    assert!(
        err.to_string().contains("decode"),
        "error names the decode failure: {err}"
    );
}

// ---- helpers ----

fn find_token(tree: &WbxmlElement, page: u8, token: u8) -> Option<&WbxmlElement> {
    if tree.page == page && tree.token == token {
        return Some(tree);
    }
    tree.children
        .iter()
        .find_map(|c| find_token(c, page, token))
}

// ---- ComposeMail response fixture (local to the compose scenarios) ----

/// A SendMail-family response with an in-body Status (any of SendMail /
/// SmartForward / SmartReply roots — 1 is success).
fn compose_status_response(root_token: u8, status: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::COMPOSE,
        root_token,
        vec![WbxmlElement::text(pages::COMPOSE, compose::STATUS, status)],
    )
}
