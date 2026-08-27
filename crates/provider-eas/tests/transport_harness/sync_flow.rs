// SPDX-License-Identifier: MPL-2.0
//! Sync scenarios ([MS-ASSYNC]): multi-page key advance (MoreAvailable),
//! the empty-body no-changes answer, SyncKey invalidation (collection
//! status 3), deletes, the client-side `Commands > Change` upsync with its
//! `Responses` correlation, and the Contacts-class seam.

use std::sync::Arc;

use provider_eas::{
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_COLLECTION, AS_COLLECTIONS, AS_COMMANDS, AS_SERVER_ID,
        AS_STATUS, AS_SYNC, AS_SYNC_KEY, EasChange, PAGE_AIRSYNC,
    },
    types::SyncRequest,
    wbxml::{WbxmlElement, WbxmlValue},
};

use super::{
    fixtures::{sync_change_response, sync_delete_response, sync_response},
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// The `<SyncKey>` text inside the request's Collection element.
fn request_sync_key(req: &CapturedRequest) -> String {
    fn find_key(el: &provider_eas::wbxml::WbxmlElement) -> Option<String> {
        if el.token == AS_SYNC_KEY
            && let WbxmlValue::Text(t) = &el.value
        {
            return Some(t.clone());
        }
        el.children.iter().find_map(find_key)
    }
    req.wbxml_tree()
        .and_then(|tree| find_key(&tree))
        .expect("request carries a SyncKey")
}

fn sync_req(collection: &str, key: &str) -> SyncRequest {
    SyncRequest {
        collection_id: collection.into(),
        sync_key: key.into(),
        class: "Email".into(),
        window_size: 25,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    }
}

/// Multi-page paging: page 1 answers `MoreAvailable` + items + a rotated
/// key; the caller (the engine's loop, played here by the test) sends the
/// rotated key on page 2, which closes the window.
#[tokio::test]
async fn sync_pages_advance_the_sync_key_until_the_window_closes() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::wbxml(&sync_response(
                "1",
                "page-1-key",
                true,
                &[("srv:1", "First page"), ("srv:2", "Second item")],
            ))
        } else {
            MockResponse::wbxml(&sync_response(
                "1",
                "page-2-key",
                false,
                &[("srv:3", "Last page")],
            ))
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());

    let page1 = client
        .sync(&sync_req("fid-inbox", "0"))
        .await
        .expect("page 1");
    assert!(page1.more_available, "page 1 must signal MoreAvailable");
    assert_eq!(page1.added.len(), 2);
    assert_eq!(page1.added[0].subject.as_deref(), Some("First page"));
    assert_eq!(request_sync_key(&server.request(1)), "0");

    let page2 = client
        .sync(&sync_req("fid-inbox", &page1.sync_key))
        .await
        .expect("page 2");
    assert!(!page2.more_available);
    assert_eq!(page2.sync_key, "page-2-key");
    assert_eq!(
        request_sync_key(&server.request(2)),
        "page-1-key",
        "page 2 must carry the rotated key"
    );
}

/// An EMPTY HTTP 200 body is a success with no changes, and the result
/// preserves the REQUEST's key (the engine cursor must not be corrupted) —
/// Android EasSync.java:225 parity.
#[tokio::test]
async fn sync_empty_body_is_no_changes_with_the_request_key() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::empty_wbxml()) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .sync(&sync_req("fid-inbox", "cursor-key-9"))
        .await
        .expect("empty body is success");
    assert_eq!(result.status, 1);
    assert_eq!(result.sync_key, "cursor-key-9", "the request key survives");
    assert!(result.added.is_empty() && result.updated.is_empty());
}

/// SyncKey invalidation: collection status 3 ([MS-ASSYNC] §2.2.3.23.3 —
/// invalid/aged sync key) surfaces on the RESULT (recovery — a fresh full
/// sync from "0" — is the caller's `recovery_action_for_sync` decision, not
/// the transport's).
#[tokio::test]
async fn sync_key_invalidation_surfaces_the_collection_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&sync_response("3", "0", false, &[]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .sync(&sync_req("fid-inbox", "stale-key"))
        .await
        .expect("status 3 is data, not an error");
    assert_eq!(result.status, 3);
    assert_eq!(result.sync_key, "0", "server demands a bootstrap from 0");
}

/// Server-side deletes ride the same response ([MS-ASSYNC] §2.2.2.4).
#[tokio::test]
async fn sync_delete_commands_surface_as_deleted_server_ids() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&sync_delete_response("del-key", "srv:gone"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .sync(&sync_req("fid-inbox", "k"))
        .await
        .expect("parse");
    assert_eq!(result.deleted_server_ids, vec!["srv:gone".to_owned()]);
}

/// The client-side Change upsync: the request carries the collection key and
/// one Change per flag mutation; the response rotates the key and
/// correlates per-item statuses through `Responses`.
#[tokio::test]
async fn sync_changes_upsync_rotates_the_key_and_reads_responses() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        let _ = req;
        MockResponse::wbxml(&sync_change_response(
            &format!("up-key-{ordinal}"),
            &[("cli-1", "1")],
        ))
    }) as Handler);
    let mut client = client_at(&server.eas_url());

    let outcome = client
        .sync_changes(
            "fid-inbox",
            "down-key-4",
            &[
                EasChange {
                    server_id: "srv:1".into(),
                    read: Some(true),
                    starred: None,
                },
                EasChange {
                    server_id: "srv:2".into(),
                    read: None,
                    starred: Some(true),
                },
            ],
        )
        .await
        .expect("upsync succeeds");
    assert_eq!(outcome.status, 1);
    assert_eq!(outcome.new_key, "up-key-1");
    // The request's Collection carried the downsync key.
    assert_eq!(request_sync_key(&server.request(1)), "down-key-4");
}

/// An EMPTY change batch makes no round-trip at all (the method is total).
#[tokio::test]
async fn sync_changes_empty_batch_never_touches_the_wire() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(500)) as Handler);
    let mut client = client_at(&server.eas_url());
    let outcome = client
        .sync_changes("fid-inbox", "k-keep", &[])
        .await
        .expect("no-op");
    assert_eq!(outcome.status, 1);
    assert_eq!(outcome.new_key, "k-keep");
    assert_eq!(server.count(), 0, "nothing sent");
}

/// A failing collection status on the upsync (status 3 = invalid key)
/// surfaces as `CommandStatus` naming the chunk.
#[tokio::test]
async fn sync_changes_invalid_key_surfaces_command_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&sync_response("3", "0", false, &[]))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .sync_changes(
            "fid-inbox",
            "stale",
            &[EasChange {
                server_id: "srv:1".into(),
                read: Some(false),
                starred: None,
            }],
        )
        .await
        .expect_err("status 3 must surface");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 3, .. }
        ),
        "expected CommandStatus 3, got {err:?}"
    );
}

/// A Contacts-class Sync response routes ApplicationData through the
/// MS-ASCNTC parser (the M8 Task 4 class seam): typed items land on
/// `contacts_added` WITH their ServerIds, and the Email-shaped vectors stay
/// empty — the class dispatch itself is the behavior under test.
#[tokio::test]
async fn sync_contacts_class_routes_to_the_typed_parser() {
    const PAGE_CONTACTS: u8 = 1;
    const CON_FIRST_NAME: u8 = 0x1F;
    const CON_LAST_NAME: u8 = 0x29;
    const CON_EMAIL_1: u8 = 0x1B;
    super::harness::init_logger();

    let app_data = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(PAGE_CONTACTS, CON_FIRST_NAME, "Alice"),
            WbxmlElement::text(PAGE_CONTACTS, CON_LAST_NAME, "Example"),
            WbxmlElement::text(PAGE_CONTACTS, CON_EMAIL_1, "alice@example.test"),
        ],
    );
    let add = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "ct:1"),
            app_data,
        ],
    );
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "ct-key"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![add]),
                ],
            )],
        )],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let mut req = sync_req("fid-contacts", "0");
    req.class = "Contacts".into();
    let result = client.sync(&req).await.expect("contacts sync parses");
    assert_eq!(result.sync_key, "ct-key");
    assert!(
        result.added.is_empty(),
        "the Email-shaped vectors stay empty on a Contacts sync"
    );
    let contact = result.contacts_added.first().expect("the typed parser ran");
    assert_eq!(contact.server_id, "ct:1");
    assert_eq!(
        contact.props.first_name.as_deref(),
        Some("Alice"),
        "the ApplicationData fields round-trip"
    );
}
