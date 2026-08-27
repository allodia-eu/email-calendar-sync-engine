// SPDX-License-Identifier: MPL-2.0
//! Multipart-envelope, MeetingResponse, and chunked MoveItems scenarios:
//! the [MS-ASCMD] §2.2.1.10.1 envelope's failure modes (no parts, corrupt
//! part 0, a dangling `itemoperations:Part` index), the MeetingResponse
//! status reads (Result form, top-level fallback, non-1), and the
//! MoveItems batch fail-fast across the 1000-move chunk boundary. Split
//! from `items_flow.rs` (the 500-line file ceiling).

use std::sync::Arc;

use provider_eas::{
    types::ItemOperationsFetchRequest,
    wbxml::{WbxmlElement, tags::pages},
};

use super::{
    fixtures::{fetch_part_response, multipart_tree},
    harness::client_at,
    items_flow::move_items_response,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// The plain fetch request the multipart scenarios issue.
fn fetch_req(collection: &str, server_id: &str) -> ItemOperationsFetchRequest {
    ItemOperationsFetchRequest {
        server_id: server_id.into(),
        collection_id: collection.into(),
        file_reference: None,
        long_id: None,
        mime: false,
        accept_multipart: true,
    }
}

/// MeetingResponse declining with a non-1 Result status surfaces as
/// `CommandStatus` with the meeting-response status table's message.
#[tokio::test]
async fn meeting_response_non_success_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        let response = WbxmlElement::container(
            pages::MREQ,
            0x07,
            vec![WbxmlElement::container(
                pages::MREQ,
                0x0A, // Result
                vec![
                    WbxmlElement::text(pages::MREQ, 0x08, "req:1"),
                    WbxmlElement::text(pages::MREQ, 0x0B, "2"), // invalid request
                ],
            )],
        );
        MockResponse::wbxml(&response)
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .meeting_response("fid-inbox", "req:1", "3", None, false)
        .await
        .expect_err("status 2 surfaces");
    let message = err.to_string();
    assert!(
        message.contains("MeetingResponse"),
        "the error names the command: {message}"
    );
}

/// MoveItems over the 1000-move chunk boundary: the FIRST chunk's failure
/// aborts the batch — chunk 2 is NEVER sent (fail-fast keeps the partial
/// window from widening).
#[tokio::test]
async fn move_items_multichunk_failure_stops_later_chunks() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&move_items_response(&[("4", None)]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let moves: Vec<(String, String, String)> = (0..1001)
        .map(|i| (format!("src:{i}"), "fid-a".into(), "fid-b".into()))
        .collect();
    let err = client.move_items(&moves).await.expect_err("chunk 1 fails");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 4, .. }
        ),
        "expected the chunk-1 failure, got {err:?}"
    );
    assert_eq!(
        server.count(),
        1,
        "1001 moves split into 2 chunks; only chunk 1 may hit the wire"
    );
}

// ---- multipart envelope failures + MeetingResponse fallback ----

/// A multipart envelope with ZERO parts cannot carry the WBXML tree — a
/// descriptive Transport error, never a panic or a silent empty parse.
#[tokio::test]
async fn multipart_with_no_parts_is_a_transport_error() {
    super::harness::init_logger();
    let server = MockServer::http(
        Arc::new(|_: &CapturedRequest, _| MockResponse::multipart(&[])) as Handler,
    );
    let mut req = fetch_req("fid-inbox", "srv:5");
    req.accept_multipart = true;
    let mut client = client_at(&server.eas_url());
    let err = client.item_operations(&req).await.expect_err("no parts");
    assert!(
        err.to_string().contains("no parts"),
        "error names the empty envelope: {err}"
    );
}

/// A multipart envelope whose part 0 is not WBXML fails through the codec
/// path with the redacted parse-failure preview.
#[tokio::test]
async fn multipart_with_corrupt_part0_fails_through_the_codec() {
    super::harness::init_logger();
    let garbage = vec![0x03, 0x01, 0x6A, 0x00, 0xFF, 0xEE, 0x00, 0x01, 0x99];
    let server = MockServer::http(Arc::new(move |_: &CapturedRequest, _| {
        MockResponse::multipart(std::slice::from_ref(&garbage.clone()))
    }) as Handler);
    let mut req = fetch_req("fid-inbox", "srv:6");
    req.accept_multipart = true;
    let mut client = client_at(&server.eas_url());
    let err = client
        .item_operations(&req)
        .await
        .expect_err("part 0 garbage");
    assert!(
        matches!(err, provider_eas::client::EasError::Wbxml(_)),
        "expected a codec error, got {err:?}"
    );
}

/// A `itemoperations:Part` reference beyond the envelope's part count is an
/// unreconcilable envelope — it fails loudly instead of dropping the body.
#[tokio::test]
async fn multipart_part_index_beyond_the_envelope_errors() {
    super::harness::init_logger();
    let tree = fetch_part_response("5");
    let server = MockServer::http(Arc::new(move |_: &CapturedRequest, _| {
        let parts = multipart_tree(&tree, b"only-part-1");
        MockResponse::multipart(&parts[..1])
    }) as Handler);
    let mut req = fetch_req("fid-inbox", "srv:7");
    req.accept_multipart = true;
    let mut client = client_at(&server.eas_url());
    let err = client
        .item_operations(&req)
        .await
        .expect_err("index 5 with 1 part");
    assert!(
        err.to_string().contains("part 5"),
        "error names the dangling index: {err}"
    );
}

/// MeetingResponse with a TOP-LEVEL Status (no Result element — off-schema
/// but accepted): the fallback read covers lenient servers.
#[tokio::test]
async fn meeting_response_top_level_status_fallback() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        let response = WbxmlElement::container(
            pages::MREQ,
            0x07,
            vec![WbxmlElement::text(pages::MREQ, 0x0B, "1")], // top-level Status
        );
        MockResponse::wbxml(&response)
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let status = client
        .meeting_response("fid-inbox", "req:1", "1", None, false)
        .await
        .expect("fallback parse");
    assert_eq!(status, 1);
}
