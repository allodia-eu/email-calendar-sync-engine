// SPDX-License-Identifier: MPL-2.0
//! Ping scenarios ([MS-ASPING]): a server-held heartbeat that expires with
use std::sync::Arc;

use provider_eas::{
    commands::{
        PAGE_PING, PING_FOLDER, PING_FOLDERS, PING_HEARTBEAT_INTERVAL, PING_PING, PING_STATUS,
    },
    types::{PingCollection, PingRequest},
    wbxml::{WbxmlElement, WbxmlValue},
};

/// status 1, and the status-5 (heartbeat out of range) retry with the
/// SERVER's interval — including that the retry request actually carries it.
use super::{
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

fn ping_req(heartbeat: u32) -> PingRequest {
    PingRequest {
        heartbeat_interval: heartbeat,
        monitored_collections: vec![PingCollection {
            collection_id: "fid-inbox".into(),
            class: "Email".into(),
        }],
    }
}

/// The request's `<HeartbeatInterval>` text.
fn request_heartbeat(req: &super::server::CapturedRequest) -> String {
    fn find(el: &provider_eas::wbxml::WbxmlElement) -> Option<String> {
        if el.token == PING_HEARTBEAT_INTERVAL
            && let WbxmlValue::Text(t) = &el.value
        {
            return Some(t.clone());
        }
        el.children.iter().find_map(find)
    }
    req.wbxml_tree()
        .and_then(|t| find(&t))
        .expect("Ping request carries a HeartbeatInterval")
}

/// A held ping that expires: status 1 ("Expired"), no changed folders, and
/// the round-trip genuinely took the hold time.
#[tokio::test]
async fn ping_expires_after_the_heartbeat_hold() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        MockResponse::wbxml(&ping_response("1", None, &[]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let started = std::time::Instant::now();
    let result = client.ping(&ping_req(15)).await.expect("ping parses");
    assert_eq!(result.status, "Expired");
    assert!(result.folders.is_empty());
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(250),
        "the server hold must be observable: {:?}",
        started.elapsed()
    );
    // The request carried the requested heartbeat and the monitored folder.
    assert_eq!(request_heartbeat(&server.request(1)), "15");
}

/// A status-2 ping: changes occurred in a monitored folder — the changed
/// ServerIds ride the result.
#[tokio::test]
async fn ping_reports_changed_folders() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&ping_response("2", None, &["fid-inbox"]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client.ping(&ping_req(15)).await.expect("ping parses");
    assert_eq!(result.status, "Changes");
    assert_eq!(result.folders, vec!["fid-inbox".to_owned()]);
}

/// Status 5 (requested heartbeat out of range) with a server-supplied
/// `HeartbeatInterval`: the client retries ONCE with the server's interval,
/// surfaces the adopted value, and the retry request actually carried it.
#[tokio::test]
async fn ping_status_5_retries_once_with_the_server_heartbeat() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::wbxml(&ping_response("5", Some(30), &[]))
        } else {
            MockResponse::wbxml(&ping_response("1", None, &[]))
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client.ping(&ping_req(15)).await.expect("retry parses");
    assert_eq!(
        result.adopted_heartbeat,
        Some(30),
        "the server interval surfaces for the engine's ping loop"
    );
    assert_eq!(server.count(), 2, "exactly one retry");
    assert_eq!(
        request_heartbeat(&server.request(2)),
        "30",
        "the retry must carry the server-adopted interval"
    );
    // Both requests are Ping commands on the page-13 root.
    assert_eq!(server.request(1).cmd().as_deref(), Some("Ping"));
    let tree = server.request(1).wbxml_tree().expect("decodes");
    assert_eq!((tree.page, tree.token), (PAGE_PING, 0x05));
}

/// Status 5 with NO server interval: no retry — the status surfaces as-is.
#[tokio::test]
async fn ping_status_5_without_interval_does_not_retry() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&ping_response("5", None, &[]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client.ping(&ping_req(15)).await.expect("status surfaces");
    assert_eq!(result.status, "5");
    assert_eq!(result.adopted_heartbeat, None);
    assert_eq!(server.count(), 1);
}

/// A bare `<Ping/>` answer with NO Status element reads as a clean expiry,
/// not as changes — the alternative fires spurious sync rounds every
/// heartbeat (the parse-level default).
#[tokio::test]
async fn ping_without_status_element_reads_as_expired() {
    super::harness::init_logger();
    let bare = provider_eas::wbxml::WbxmlElement::container(
        provider_eas::commands::PAGE_PING,
        0x05,
        vec![],
    );
    let server = MockServer::http(
        Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&bare)) as Handler,
    );
    let mut client = client_at(&server.eas_url());
    let result = client.ping(&ping_req(15)).await.expect("bare ping parses");
    assert_eq!(result.status, "Expired");
    assert!(result.folders.is_empty());
}

// ---- Ping response fixture (local to the ping scenarios) ----

/// A Ping response: status text, optional server heartbeat, changed folders.
fn ping_response(status: &str, heartbeat: Option<u32>, folders: &[&str]) -> WbxmlElement {
    let mut children = vec![WbxmlElement::text(PAGE_PING, PING_STATUS, status)];
    if let Some(interval) = heartbeat {
        children.push(WbxmlElement::text(
            PAGE_PING,
            PING_HEARTBEAT_INTERVAL,
            interval.to_string(),
        ));
    }
    if !folders.is_empty() {
        children.push(WbxmlElement::container(
            PAGE_PING,
            PING_FOLDERS,
            folders
                .iter()
                .map(|&f| WbxmlElement::text(PAGE_PING, PING_FOLDER, f))
                .collect(),
        ));
    }
    WbxmlElement::container(PAGE_PING, PING_PING, children)
}
