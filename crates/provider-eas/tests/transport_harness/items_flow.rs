// SPDX-License-Identifier: MPL-2.0
//! ItemOperations + misc command scenarios over the real transport: inline
//! Fetch, Body fetch, the multipart opt-in ([MS-ASCMD] §2.2.1.10.1) with
//! `itemoperations:Part` reassembly, MoveItems (status 3 = success),
//! MeetingResponse, GetItemEstimate, Search, EmptyFolderContents, and
//! ConversationMove.

use std::sync::Arc;

use provider_eas::{
    types::{
        ConversationMoveRequest, EmptyFolderContentsRequest, GetItemEstimateRequest,
        ItemOperationsFetchRequest, SearchRequest,
    },
    wbxml::{
        WbxmlElement,
        tags::{
            base, gal, item_operations, pages,
            search::{self as sr},
        },
    },
};

use super::{
    fixtures::{fetch_body_response, fetch_part_response, fetch_response, multipart_tree},
    harness::{b64_encode, client_at},
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

fn fetch_req(collection: &str, server_id: &str) -> ItemOperationsFetchRequest {
    ItemOperationsFetchRequest {
        server_id: server_id.into(),
        collection_id: collection.into(),
        file_reference: None,
        long_id: None,
        mime: false,
        accept_multipart: false,
    }
}

/// An inline Properties > Data fetch: base64 text body + content type.
#[tokio::test]
async fn item_operations_fetches_an_inline_body() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&fetch_response("aGVsbG8=", "text/plain"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .item_operations(&fetch_req("fid-inbox", "srv:1"))
        .await
        .expect("fetch parses");
    assert_eq!(result.status, 1);
    assert_eq!(result.data.as_deref(), Some("aGVsbG8="));
    assert_eq!(result.content_type.as_deref(), Some("text/plain"));
    // The request asked for an HTML body via BodyPreference (page 17).
    let tree = server.request(1).wbxml_tree().expect("request decodes");
    assert!(tree_contains(&tree, pages::BASE, base::BODY_PREFERENCE));
}

/// An item/body fetch answer: airsyncbase:Body with Type + Data.
#[tokio::test]
async fn item_operations_fetches_a_typed_body() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&fetch_body_response("2", "PGI+aGk8L2I+"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .item_operations(&fetch_req("fid-inbox", "srv:2"))
        .await
        .expect("fetch parses");
    assert_eq!(result.data.as_deref(), Some("PGI+aGk8L2I+"));
    assert_eq!(
        result.content_type.as_deref(),
        Some("text/html"),
        "Type 2 maps to text/html when no ContentType was sent"
    );
}

/// The multipart opt-in: the request carries `MS-ASAcceptMultiPart: T`, the
/// answer is a `application/vnd.ms-sync.multipart` envelope, and the
/// `itemoperations:Part` reference is resolved into an inline base64 Data —
/// the payload bytes the server sent as part 1.
#[tokio::test]
async fn item_operations_multipart_part_is_reassembled_inline() {
    super::harness::init_logger();
    let payload: &[u8] = b"\x89PNG\r\n\x1a\nbinary-attachment-bytes";
    let server = MockServer::http(Arc::new(move |_: &CapturedRequest, _| {
        let tree = fetch_part_response("1");
        MockResponse::multipart(&multipart_tree(&tree, payload))
    }) as Handler);
    let mut req = fetch_req("fid-inbox", "srv:3");
    req.accept_multipart = true;
    let mut client = client_at(&server.eas_url());
    let result = client
        .item_operations(&req)
        .await
        .expect("multipart parses");
    // Part 1's bytes surface as base64 through the same `data` field.
    assert_eq!(result.data.as_deref(), Some(b64_encode(payload).as_str()));
    // The opt-in header actually went out.
    assert_eq!(
        server.request(1).header("ms-asacceptmultipart"),
        Some("T"),
        "the request must carry MS-ASAcceptMultiPart: T"
    );
}

/// A multipart content-type WITHOUT the request opt-in is a protocol
/// violation — Transport error, never parsed.
#[tokio::test]
async fn unrequested_multipart_is_rejected() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        let tree = fetch_part_response("1");
        MockResponse::multipart(&multipart_tree(&tree, b"part-bytes"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .item_operations(&fetch_req("fid-inbox", "srv:4"))
        .await
        .expect_err("unrequested multipart must error");
    assert!(
        err.to_string().contains("MS-ASAcceptMultiPart"),
        "error names the missing opt-in: {err}"
    );
}

/// MoveItems: the INVERTED status table — per-Move Status 3 WITH a DstMsgId
/// is SUCCESS ([MS-ASCMD] 2.2.3.177.10, Exchange 15.2 live evidence).
#[tokio::test]
async fn move_items_status_3_with_dst_id_is_success() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&move_items_response(&[("3", Some("dst:1"))]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let results = client
        .move_items(&[("src:1".into(), "fid-a".into(), "fid-b".into())])
        .await
        .expect("status 3 + DstMsgId is success");
    assert_eq!(results, vec![(3, Some("dst:1".to_owned()))]);
}

/// MoveItems with a failing per-move status (4 = source or destination
/// folder not found) surfaces as `CommandStatus` — later chunks would not
/// be sent (fail-fast).
#[tokio::test]
async fn move_items_failure_surfaces_command_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&move_items_response(&[("4", None)]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .move_items(&[("bad:1".into(), "fid-a".into(), "fid-b".into())])
        .await
        .expect_err("per-move status 4 must surface");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 4, .. }
        ),
        "expected CommandStatus 4, got {err:?}"
    );
}

/// An empty MoveItems batch never touches the wire.
#[tokio::test]
async fn move_items_empty_batch_sends_nothing() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let mut client = client_at(&server.eas_url());
    let results = client.move_items(&[]).await.expect("empty is a no-op");
    assert!(results.is_empty());
    assert_eq!(server.count(), 0);
}

/// MeetingResponse accept: Result status 1.
#[tokio::test]
async fn meeting_response_accept_returns_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        let response = WbxmlElement::container(
            pages::MREQ,
            0x07, // MeetingResponse root
            vec![WbxmlElement::container(
                pages::MREQ,
                0x0A, // Result
                vec![
                    WbxmlElement::text(pages::MREQ, 0x08, "req:1"), // RequestId
                    WbxmlElement::text(pages::MREQ, 0x0B, "1"),     // Status
                ],
            )],
        );
        MockResponse::wbxml(&response)
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let status = client
        .meeting_response("fid-inbox", "req:1", "1", None, false)
        .await
        .expect("accept parses");
    assert_eq!(status, 1);
}

/// GetItemEstimate: Response > Status + Collection > Estimate.
#[tokio::test]
async fn get_item_estimate_reads_the_estimate() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        let response = WbxmlElement::container(
            pages::GIE,
            0x05,
            vec![WbxmlElement::container(
                pages::GIE,
                0x0D, // Response
                vec![
                    WbxmlElement::text(pages::GIE, 0x0E, "1"), // Status
                    WbxmlElement::container(
                        pages::GIE,
                        0x08, // Collection
                        vec![
                            WbxmlElement::text(pages::GIE, 0x0A, "fid-inbox"), // CollectionId
                            WbxmlElement::text(pages::GIE, 0x0C, "42"),        // Estimate
                        ],
                    ),
                ],
            )],
        );
        MockResponse::wbxml(&response)
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .get_item_estimate(&GetItemEstimateRequest {
            collection_id: "fid-inbox".into(),
            sync_key: "k".into(),
            class: "Email".into(),
            filter_age_days: 0,
        })
        .await
        .expect("estimate parses");
    assert_eq!(result.status, 1);
    assert_eq!(result.collection_id, "fid-inbox");
    assert_eq!(result.count, 42);
}

/// Search (GAL store): Response > Store > Result > Properties.
#[tokio::test]
async fn search_reads_gal_results() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        let response = WbxmlElement::container(
            sr::PAGE,
            sr::SEARCH,
            vec![
                WbxmlElement::text(sr::PAGE, sr::STATUS, "1"),
                WbxmlElement::container(
                    sr::PAGE,
                    sr::RESPONSE,
                    vec![WbxmlElement::container(
                        sr::PAGE,
                        sr::STORE,
                        vec![
                            WbxmlElement::text(sr::PAGE, sr::TOTAL, "1"),
                            WbxmlElement::container(
                                sr::PAGE,
                                sr::RESULT,
                                vec![WbxmlElement::container(
                                    sr::PAGE,
                                    sr::PROPERTIES,
                                    vec![
                                        WbxmlElement::text(
                                            gal::PAGE,
                                            gal::DISPLAY_NAME,
                                            "Alice Example",
                                        ),
                                        WbxmlElement::text(
                                            gal::PAGE,
                                            gal::EMAIL_ADDRESS,
                                            "alice@example.test",
                                        ),
                                    ],
                                )],
                            ),
                        ],
                    )],
                ),
            ],
        );
        MockResponse::wbxml(&response)
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .search(&SearchRequest {
            store: "GAL".into(),
            query: "alice".into(),
            collection_id: None,
            range: "0-49".into(),
            deep_traversal: false,
        })
        .await
        .expect("search parses");
    assert_eq!(result.status, 1);
    assert_eq!(result.total, Some(1));
    assert_eq!(result.results.len(), 1);
    let gal_entry = result.results[0].gal.as_ref().expect("GAL row");
    assert_eq!(gal_entry.display_name.as_deref(), Some("Alice Example"));
    assert_eq!(
        gal_entry.email_address.as_deref(),
        Some("alice@example.test")
    );
}

/// EmptyFolderContents: itemoperations-level Status 1.
#[tokio::test]
async fn empty_folder_contents_reports_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&item_ops_status_response("1"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .empty_folder_contents(&EmptyFolderContentsRequest {
            collection_id: "fid-junk".into(),
            delete_sub_folders: false,
        })
        .await
        .expect("empty parses");
    assert_eq!(result.status, 1);
}

/// ConversationMove: itemoperations-level Status 1.
#[tokio::test]
async fn conversation_move_reports_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&item_ops_status_response("1"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .conversation_move(&ConversationMoveRequest {
            dst_folder_id: "fid-archive".into(),
            conversation_id: vec![0x01, 0x02],
            move_always: false,
        })
        .await
        .expect("move parses");
    assert_eq!(result.status, 1);
}

// ---- tiny tree helpers ----

/// A bare `<ItemOperations><Status>…</Status></ItemOperations>` response.
fn item_ops_status_response(status: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::ITEMS, // ItemOperations page 20 (tags::pages stops at FIND)
        item_operations::ITEM_OPERATIONS,
        vec![WbxmlElement::text(
            pages::ITEMS,
            item_operations::STATUS,
            status,
        )],
    )
}

/// A MoveItems response: one `Response` per `(status, dst)` pair.
pub(crate) fn move_items_response(moves: &[(&str, Option<&str>)]) -> WbxmlElement {
    let responses = moves
        .iter()
        .map(|(status, dst)| {
            let mut children = vec![WbxmlElement::text(pages::MOVE, 0x0B, *status)]; // Status
            if let Some(dst) = dst {
                children.push(WbxmlElement::text(pages::MOVE, 0x0C, *dst)); // DstMsgId
            }
            WbxmlElement::container(pages::MOVE, 0x0A, children) // Response
        })
        .collect();
    WbxmlElement::container(pages::MOVE, 0x05, responses) // MoveItems root
}

fn tree_contains(tree: &WbxmlElement, page: u8, token: u8) -> bool {
    if tree.page == page && tree.token == token {
        return true;
    }
    tree.children.iter().any(|c| tree_contains(c, page, token))
}

// ---- non-1 status gates + chunked MoveItems fail-fast ----

/// EmptyFolderContents answering non-1 surfaces as `CommandStatus`.
#[tokio::test]
async fn empty_folder_contents_non_success_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&item_ops_status_response("151"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .empty_folder_contents(&EmptyFolderContentsRequest {
            collection_id: "fid-junk".into(),
            delete_sub_folders: false,
        })
        .await
        .expect_err("status 151 surfaces");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 151, .. }
        ),
        "expected CommandStatus 151, got {err:?}"
    );
}

/// ConversationMove answering non-1 surfaces as `CommandStatus`.
#[tokio::test]
async fn conversation_move_non_success_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&item_ops_status_response("16"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .conversation_move(&ConversationMoveRequest {
            dst_folder_id: "fid-archive".into(),
            conversation_id: vec![0x01],
            move_always: true,
        })
        .await
        .expect_err("status 16 surfaces");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 16, .. }
        ),
        "expected CommandStatus 16, got {err:?}"
    );
}
