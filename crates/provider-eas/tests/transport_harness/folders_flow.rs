// SPDX-License-Identifier: MPL-2.0
//! FolderSync + folder-op scenarios ([MS-ASFolderSync]): hierarchy sync with
//! key caching, the bootstrap "0" key, folder create/delete/update adopting
//! the rotated hierarchy key, and the FolderSync 108 error path.

use std::sync::Arc;

use provider_eas::{
    commands::{FH_FOLDER_SYNC, FH_STATUS, FH_SYNC_KEY, PAGE_FOLDER},
    types::{FolderCreateRequest, FolderDeleteRequest, FolderUpdateRequest},
    wbxml::{WbxmlElement, WbxmlValue},
};

use super::{
    fixtures::{
        FOLDER_CREATE_ROOT, FOLDER_DELETE_ROOT, FOLDER_UPDATE_ROOT, folder_op_response,
        folder_sync_response, folder_sync_status,
    },
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// The `<SyncKey>` text of a decoded request tree.
fn request_sync_key(req: &CapturedRequest) -> String {
    let tree = req.wbxml_tree().expect("request body decodes");
    tree.children
        .iter()
        .find(|c| c.token == FH_SYNC_KEY)
        .and_then(|c| match &c.value {
            WbxmlValue::Text(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no SyncKey in request tree"))
}

/// FolderSync with the bootstrap key "0" returns changes and caches the
/// rotated hierarchy key; the NEXT FolderSync sends it back.
#[tokio::test]
async fn folder_sync_caches_and_reuses_the_hierarchy_key() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, ordinal: usize| {
        MockResponse::wbxml(&folder_sync_response(
            &format!("fs-key-{ordinal}"),
            &[
                ("fid-inbox", "0", "Inbox", "2"),
                ("fid-drafts", "0", "Drafts", "3"),
            ],
        ))
    }) as Handler);
    let mut client = client_at(&server.eas_url());

    let first = client.folder_sync("0").await.expect("first FolderSync");
    assert_eq!(first.sync_key, "fs-key-1");
    assert_eq!(first.changes.len(), 2);
    assert_eq!(first.changes[0].display_name, "Inbox");
    assert_eq!(
        request_sync_key(&server.request(1)),
        "0",
        "bootstrap key on round 1"
    );

    // Second round: the client sends its CACHED key, not "0".
    let cached_key = client.hierarchy_key().to_owned();
    let second = client
        .folder_sync(&cached_key)
        .await
        .expect("second FolderSync");
    assert_eq!(second.sync_key, "fs-key-2");
    assert_eq!(
        request_sync_key(&server.request(2)),
        "fs-key-1",
        "second round must send the cached key"
    );
    assert_eq!(client.hierarchy_sync_key_str(), "fs-key-2");
}

/// FolderSync answering top-level 108 (device ID missing/invalid) surfaces
/// as `CommandStatus` with the decoded message.
#[tokio::test]
async fn folder_sync_status_108_surfaces_as_command_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&folder_sync_status("108"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client.folder_sync("0").await.expect_err("108 must surface");
    match err {
        provider_eas::client::EasError::CommandStatus { status, message } => {
            assert_eq!(status, 108);
            assert!(
                message.contains("FolderSync"),
                "context names the command: {message}"
            );
        }
        other => panic!("expected CommandStatus, got {other:?}"),
    }
}

/// FolderCreate/Delete/Update each adopt the rotated hierarchy SyncKey the
/// response carries ([MS-ASCMD] 2.2.3.181.1) — the next folder op sends it.
#[tokio::test]
async fn folder_ops_adopt_the_rotated_hierarchy_key() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        match req.cmd().as_deref() {
            Some("FolderCreate") => MockResponse::wbxml(&folder_op_response(
                FOLDER_CREATE_ROOT,
                "1",
                &format!("hier-{ordinal}"),
            )),
            Some("FolderDelete") => MockResponse::wbxml(&folder_op_response(
                FOLDER_DELETE_ROOT,
                "1",
                &format!("hier-{ordinal}"),
            )),
            Some("FolderUpdate") => MockResponse::wbxml(&folder_op_response(
                FOLDER_UPDATE_ROOT,
                "1",
                &format!("hier-{ordinal}"),
            )),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());

    let (status, _new_id) = client
        .folder_create(&FolderCreateRequest {
            parent_id: "0".into(),
            display_name: "Archive".into(),
            class: "Email".into(),
        })
        .await
        .expect("FolderCreate");
    assert_eq!(status, 1);
    assert_eq!(client.hierarchy_key(), "hier-1");

    // The Delete request sends the key the Create response rotated to.
    client
        .folder_delete(&FolderDeleteRequest {
            server_id: "fid-1".into(),
        })
        .await
        .expect("FolderDelete");
    assert_eq!(
        request_sync_key(&server.request(2)),
        "hier-1",
        "FolderDelete must send the Create-rotated key"
    );

    client
        .folder_update(&FolderUpdateRequest {
            server_id: "fid-2".into(),
            parent_id: Some("0".into()),
            display_name: Some("Renamed".into()),
        })
        .await
        .expect("FolderUpdate");
    assert_eq!(client.hierarchy_key(), "hier-3");
}

/// A folder-op answering non-1 still parses; the status rides the tuple.
#[tokio::test]
async fn folder_create_non_success_status_returns_the_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&folder_op_response(FOLDER_CREATE_ROOT, "4", "no-key"))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let (status, new_id) = client
        .folder_create(&FolderCreateRequest {
            parent_id: "0".into(),
            display_name: "Archive".into(),
            class: "Email".into(),
        })
        .await
        .expect("parse succeeds");
    assert_eq!(status, 4, "the folder-op status surfaces on the tuple");
    assert!(new_id.is_none());
}

/// An OPAQUE (non-inline-text) SyncKey decodes as UTF-8 — servers MAY send
/// large text payloads as OPAQUE, and the tree reader must accept the form.
#[tokio::test]
async fn folder_sync_opaque_sync_key_decodes() {
    super::harness::init_logger();
    let response = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![
            WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
            WbxmlElement::opaque(PAGE_FOLDER, FH_SYNC_KEY, b"opaque-key-9".to_vec()),
        ],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let result = client
        .folder_sync("0")
        .await
        .expect("opaque SyncKey parses");
    assert_eq!(result.sync_key, "opaque-key-9");
    assert_eq!(client.hierarchy_key(), "opaque-key-9");
}

/// A SyncKey whose OPAQUE bytes are NOT valid UTF-8 is a codec error — the
/// tree reader refuses rather than lossily converting a cursor.
#[tokio::test]
async fn folder_sync_non_utf8_opaque_sync_key_errors() {
    super::harness::init_logger();
    let response = WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![
            WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
            WbxmlElement::opaque(PAGE_FOLDER, FH_SYNC_KEY, vec![0xFF, 0xFE, 0x00, 0x81]),
        ],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("non-UTF-8 cursor refuses");
    assert!(
        matches!(err, provider_eas::client::EasError::Wbxml(_)),
        "expected a codec error, got {err:?}"
    );
    assert!(
        err.to_string().contains("non-UTF-8"),
        "the error names the encoding failure: {err}"
    );
}
