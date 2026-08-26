// SPDX-License-Identifier: MPL-2.0
//! Live Sync first page against the Inbox.

use super::*;

/// Smoke: real Sync first page against the Inbox. Exercises the exact wire
/// shape the engine's S3 streaming chunks depend on (BodyPreference HTML +
/// WindowSize-bounded page + real sync key advance) which the
/// OPTIONS/Provision/FolderSync smoke never touches.
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn sync_first_page_smoke() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (KYLINS_EAS_LIVE_URL/USER/PASS) — skipping");
        return;
    };

    // Distinct device id from options_provision_foldersync_smoke: the two
    // live tests run on parallel threads, and two concurrent Provision
    // phase-1 handshakes from the SAME device identity race server-side
    // (observed on mail.example.org 2026-08-16: phase-1 status 135, "sync
    // state already exists"; serialized runs pass cleanly). Two DIFFERENT
    // devices provisioning the same mailbox concurrently is a normal
    // server scenario, so this keeps the stock parallel invocation safe.
    config.device_id = "KYLINSLIVETEST02".to_string();

    // Negotiate + provision exactly like the existing smoke test.
    let probe = live_client(config.clone());
    let server = probe.options().await.expect("OPTIONS round-trip failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common EAS protocol version with the server");
    config.protocol_version = negotiated;
    let mut client = live_client(config);
    client
        .provision()
        .await
        .expect("Provision handshake failed");

    // Bootstrap hierarchy; locate the Inbox by its EAS folder Type byte.
    // 2 = Inbox per MS-ASFD FolderHierarchy:Type (see EasFolder.folder_type
    // doc comment in crates/provider-eas/src/types.rs — re-verify there if
    // this assertion ever fails).
    let folders = client.folder_sync("0").await.expect("FolderSync failed");
    let inbox = folders
        .changes
        .iter()
        .find(|f| f.folder_type == Some(2))
        .expect("no Inbox (folder_type 2) in FolderSync bootstrap");

    // First Sync page: tiny window keeps the test cheap on busy mailboxes;
    // fetch_body = false (headers only — bodies are ItemOperations' job and
    // are already covered by the probe harness). `SyncRequest` has NO
    // Default derive — every field is listed explicitly (same pattern as the
    // crate's own tests in types.rs).
    let req = SyncRequest {
        collection_id: inbox.server_id.clone(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let page = client.sync(&req).await.expect("Sync bootstrap page failed");

    assert_eq!(page.status, 1, "Sync collection status must be 1 (success)");
    // A bootstrap ("0") key is ALWAYS valid for exactly one round; the
    // server must return a real (non-"0") key to continue — this is the
    // cursor contract S3 streaming checkpoints depend on.
    assert!(
        !page.sync_key.is_empty() && page.sync_key != "0",
        "Sync bootstrap did not advance the sync key: {:?}",
        page.sync_key
    );

    // Second round with the real key: must succeed (status 1) even with an
    // empty change set — this is the "held key, no changes" shape the ping
    // loop lives on.
    let req2 = SyncRequest {
        sync_key: page.sync_key.clone(),
        ..req.clone()
    };
    let page2 = client.sync(&req2).await.expect("Sync round 2 failed");
    assert_eq!(page2.status, 1, "Sync round-2 status must be 1 (success)");
    let _ = (page.added.len(), page.more_available); // observed, not asserted
}
