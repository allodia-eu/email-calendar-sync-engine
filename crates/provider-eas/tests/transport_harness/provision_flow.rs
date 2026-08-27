// SPDX-License-Identifier: MPL-2.0
//! Provision scenarios ([MS-ASPROV] + the retry layer's provision branches):
//! the two-phase handshake, HTTP 449 → Provision re-issue, in-body 142 →
//! Provision re-issue, the 165 DeviceInformation bootstrap, RemoteWipe
//! refusal, and non-1 phase statuses.

use std::sync::Arc;

use provider_eas::wbxml::tags::{pages, provision};

use super::{
    fixtures::{
        folder_sync_response, folder_sync_status, provision_remote_wipe, provision_response,
    },
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// Distinguish Provision phase 1 (PolicyType only) from phase 2 (carries the
/// temp PolicyKey) by decoding the request body — the way a real server
/// routes them.
fn has_policy_key(el: &provider_eas::wbxml::WbxmlElement) -> bool {
    el.children
        .iter()
        .any(|c| c.token == provision::POLICY_KEY || has_policy_key(c))
}

fn is_provision_phase2(req: &CapturedRequest) -> bool {
    let tree = req.wbxml_tree().expect("provision request decodes");
    has_policy_key(&tree)
}

/// A Provision-then-command handler: two-phase Provision with the given
/// permanent key, then a successful FolderSync.
fn provision_then_folder_sync(perm_key: &'static str) -> Handler {
    Arc::new(
        move |req: &CapturedRequest, _ordinal: usize| match req.cmd().as_deref() {
            Some("Provision") => {
                if is_provision_phase2(req) {
                    MockResponse::wbxml(&provision_response("1", perm_key))
                } else {
                    MockResponse::wbxml(&provision_response("1", "temp-key-1"))
                }
            }
            _ => MockResponse::wbxml(&folder_sync_response(
                "fs-key-2",
                &[("fid-inbox", "0", "Inbox", "2")],
            )),
        },
    )
}

/// The plain two-phase handshake: phase 1 issues a temp key, phase 2 the
/// permanent key, and the client caches it for the X-MS-PolicyKey header.
#[tokio::test]
async fn provision_two_phase_rotates_the_policy_key() {
    super::harness::init_logger();
    let server = MockServer::http(provision_then_folder_sync("perm-123"));
    let mut client = client_at(&server.eas_url());

    client.provision().await.expect("two-phase handshake");
    assert_eq!(client.policy_key(), "perm-123", "permanent key cached");

    // Both Provision requests went out; the phase-2 request carried the
    // temp key from phase 1.
    let cmds: Vec<Option<String>> = server.captured().iter().map(CapturedRequest::cmd).collect();
    assert_eq!(
        cmds,
        vec![Some("Provision".into()), Some("Provision".into())]
    );
    let phase2_tree = server.request(2).wbxml_tree().expect("phase 2 decodes");
    let body = policy_key_text(&phase2_tree);
    assert!(body.contains("temp-key-1"), "phase 2 acks the temp key");

    client
        .folder_sync("0")
        .await
        .expect("FolderSync after provision");
    assert_eq!(
        server.request(3).policy_key(),
        "perm-123",
        "subsequent commands send the rotated key"
    );
}

fn policy_key_text(tree: &provider_eas::wbxml::WbxmlElement) -> String {
    fn walk(el: &provider_eas::wbxml::WbxmlElement, out: &mut String) {
        if el.token == provision::POLICY_KEY
            && let provider_eas::wbxml::WbxmlValue::Text(t) = &el.value
        {
            out.push_str(t);
        }
        for c in &el.children {
            walk(c, out);
        }
    }
    let mut s = String::new();
    walk(tree, &mut s);
    s
}

/// HTTP 449 ([MS-ASHTTP] §2.2.1.1.2): the retry layer runs the full
/// Provision handshake and re-issues the ORIGINAL command once — no
/// recursion (the Provision commands themselves ride the no-retry path).
#[tokio::test]
async fn http_449_triggers_provision_then_command_reissue() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(move |req: &CapturedRequest, ordinal: usize| {
        match req.cmd().as_deref() {
            Some("FolderSync") if ordinal == 1 => {
                // The first FolderSync is refused with 449 — no body.
                MockResponse::bare(449)
            }
            Some("Provision") => {
                if is_provision_phase2(req) {
                    MockResponse::wbxml(&provision_response("1", "perm-449"))
                } else {
                    MockResponse::wbxml(&provision_response("1", "temp-449"))
                }
            }
            _ => MockResponse::wbxml(&folder_sync_response(
                "fs-after-449",
                &[("fid-1", "0", "Inbox", "2")],
            )),
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());

    let result = client.folder_sync("0").await.expect("retry succeeds");
    assert_eq!(result.sync_key, "fs-after-449");
    assert_eq!(client.policy_key(), "perm-449");

    // Request order: FolderSync(449) → Provision phase1 → Provision phase2 →
    // FolderSync(retried). The retried command carries the new policy key.
    let order: Vec<Option<String>> = server.captured().iter().map(CapturedRequest::cmd).collect();
    assert_eq!(
        order,
        vec![
            Some("FolderSync".into()),
            Some("Provision".into()),
            Some("Provision".into()),
            Some("FolderSync".into())
        ]
    );
    assert_eq!(server.request(1).policy_key(), "0");
    assert_eq!(server.request(4).policy_key(), "perm-449");
}

/// In-body top-level Status 142 (Common: device not provisioned): the public
/// `send_command` wrapper runs Provision once and re-issues the command —
/// the second-layer retry [MS-ASCMD] §2.2.3.177.16.
#[tokio::test]
async fn in_body_status_142_triggers_provision_and_reissue() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(move |req: &CapturedRequest, ordinal: usize| {
        match req.cmd().as_deref() {
            Some("FolderSync") if ordinal == 1 => {
                // 200 + WBXML body whose top-level Status is 142.
                MockResponse::wbxml(&folder_sync_status("142"))
            }
            Some("Provision") => {
                if is_provision_phase2(req) {
                    MockResponse::wbxml(&provision_response("1", "perm-142"))
                } else {
                    MockResponse::wbxml(&provision_response("1", "temp-142"))
                }
            }
            _ => MockResponse::wbxml(&folder_sync_response("fs-142", &[])),
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());

    let result = client.folder_sync("0").await.expect("retried FolderSync");
    assert_eq!(result.sync_key, "fs-142");
    assert_eq!(client.policy_key(), "perm-142");
    assert_eq!(server.count(), 4, "FS(142) → Prov ×2 → FS(retry)");
}

/// Provision phase 1 answering 165 (DeviceInformationRequired) makes the
/// client send the standalone Settings DeviceInformation command and retry
/// phase 1 once ([MS-ASPROV] product behavior; Exchange 2019).
#[tokio::test]
async fn provision_165_sends_device_information_and_retries_phase1() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(move |req: &CapturedRequest, ordinal: usize| {
        match req.cmd().as_deref() {
            Some("Provision") if ordinal == 1 => {
                MockResponse::wbxml(&provision_response("165", ""))
            }
            Some("Settings") => MockResponse::wbxml(&super::fixtures::settings_response(
                super::fixtures::device_information_element("1"),
            )),
            Some("Provision") if ordinal == 3 => {
                MockResponse::wbxml(&provision_response("1", "temp-165"))
            }
            Some("Provision") => MockResponse::wbxml(&provision_response("1", "perm-165")),
            _ => MockResponse::bare(500),
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());

    client.provision().await.expect("165 bootstrap recovers");
    assert_eq!(client.policy_key(), "perm-165");
    let order: Vec<Option<String>> = server.captured().iter().map(CapturedRequest::cmd).collect();
    assert_eq!(
        order,
        vec![
            Some("Provision".into()), // 165
            Some("Settings".into()),  // DeviceInformation
            Some("Provision".into()), // phase 1 retry
            Some("Provision".into()), // phase 2
        ]
    );
    // The DI request named the device model/type from the config.
    let settings_body = server.request(2).wbxml_tree().expect("Settings decodes");
    assert_eq!(settings_body.page, pages::SETTINGS);
}

/// A phase-1 RemoteWipe demand is REFUSED: CommandStatus 140, never executed.
#[tokio::test]
async fn provision_remote_wipe_is_refused() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _ordinal: usize| {
        if req.cmd().as_deref() == Some("Provision") && !is_provision_phase2(req) {
            MockResponse::wbxml(&provision_remote_wipe())
        } else {
            MockResponse::bare(500)
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .provision()
        .await
        .expect_err("RemoteWipe must refuse");
    match err {
        provider_eas::client::EasError::CommandStatus { status, message } => {
            assert_eq!(status, 140);
            assert!(
                message.contains("RemoteWipe"),
                "message names the refusal: {message}"
            );
        }
        other => panic!("expected CommandStatus 140, got {other:?}"),
    }
    assert_eq!(server.count(), 1, "phase 2 never runs after a wipe demand");
}

/// A non-1 phase-1 status surfaces as CommandStatus with the phase named.
#[tokio::test]
async fn provision_phase1_non_success_status_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&provision_response("2", ""))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .provision()
        .await
        .expect_err("phase-1 status 2 must surface");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 2, .. }
        ),
        "expected CommandStatus 2, got {err:?}"
    );
}

/// Phase 1 succeeding without a PolicyKey is a Transport error (nothing to
/// ack in phase 2).
#[tokio::test]
async fn provision_phase1_without_policy_key_errors() {
    super::harness::init_logger();
    let tree = provider_eas::wbxml::WbxmlElement::container(
        pages::PROVISION,
        provision::PROVISION,
        vec![provider_eas::wbxml::WbxmlElement::text(
            pages::PROVISION,
            provision::STATUS,
            "1",
        )],
    );
    let server = MockServer::http(
        Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&tree)) as Handler,
    );
    let mut client = client_at(&server.eas_url());
    let err = client.provision().await.expect_err("no key must error");
    assert!(
        err.to_string().contains("no PolicyKey"),
        "error must name the missing key: {err}"
    );
}

// ---- phase-2 failure arms ----

/// Phase 2 answering a non-1 status surfaces as `CommandStatus` naming the
/// phase; the permanent key is NOT cached.
#[tokio::test]
async fn provision_phase2_non_success_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(move |req: &CapturedRequest, _| {
        if req.cmd().as_deref() == Some("Provision") && !is_provision_phase2(req) {
            MockResponse::wbxml(&provision_response("1", "temp-p2"))
        } else {
            MockResponse::wbxml(&provision_response("2", "never-issued"))
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .provision()
        .await
        .expect_err("phase-2 status 2 surfaces");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 2, .. }
        ),
        "expected CommandStatus 2, got {err:?}"
    );
    assert_eq!(
        client.policy_key(),
        "",
        "nothing cached on a failed handshake"
    );
}

/// Phase 2 succeeding WITHOUT a permanent PolicyKey element is a Transport
/// error — there is nothing to ack future commands with.
#[tokio::test]
async fn provision_phase2_without_permanent_key_errors() {
    super::harness::init_logger();
    let bare_ack = provider_eas::wbxml::WbxmlElement::container(
        pages::PROVISION,
        provision::PROVISION,
        vec![provider_eas::wbxml::WbxmlElement::text(
            pages::PROVISION,
            provision::STATUS,
            "1",
        )],
    );
    let server = MockServer::http(Arc::new(move |req: &CapturedRequest, _| {
        if req.cmd().as_deref() == Some("Provision") && !is_provision_phase2(req) {
            MockResponse::wbxml(&provision_response("1", "temp-ok"))
        } else {
            // Status 1 but no PolicyKey element at all.
            MockResponse::wbxml(&bare_ack)
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .provision()
        .await
        .expect_err("missing permanent key errors");
    assert!(
        err.to_string().contains("permanent PolicyKey"),
        "error names the missing key: {err}"
    );
}

/// A RemoteWipe demand in PHASE 2 is equally refused — never auto-executed.
#[tokio::test]
async fn provision_phase2_remote_wipe_is_refused() {
    super::harness::init_logger();
    let wipe = provision_remote_wipe();
    let server = MockServer::http(Arc::new(move |req: &CapturedRequest, _| {
        if req.cmd().as_deref() == Some("Provision") && !is_provision_phase2(req) {
            MockResponse::wbxml(&provision_response("1", "temp-wipe"))
        } else {
            MockResponse::wbxml(&wipe)
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client.provision().await.expect_err("phase-2 wipe refuses");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 140, .. }
        ),
        "expected CommandStatus 140, got {err:?}"
    );
}

/// Provision phase 1 answering 165 twice in a row still surfaces the
/// SETTINGS error when the DeviceInformation ack itself fails (status 2):
/// the bootstrap cannot loop forever.
#[tokio::test]
async fn provision_165_with_failing_device_information_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        if req.cmd().as_deref() == Some("Provision") {
            MockResponse::wbxml(&provision_response("165", ""))
        } else {
            // Settings DeviceInformation rejected with a top-level status 2.
            MockResponse::wbxml(&super::fixtures::settings_response(
                super::fixtures::device_information_element("1"),
            ))
            .with_header("MS-ASProtocolStatus", "2")
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client.provision().await.expect_err("DI failure surfaces");
    let message = err.to_string();
    assert!(
        message.contains("protocol error") || message.contains("Settings"),
        "the DI rejection surfaces: {message}"
    );
}
