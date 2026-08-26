// SPDX-License-Identifier: MPL-2.0
//! OPTIONS negotiation, two-phase Provision, and FolderSync bootstrap against a live server.

use super::*;

/// Smoke: OPTIONS version negotiation → two-phase Provision handshake →
/// bootstrap FolderSync. Exercises the whole transport stack (WBXML
/// serialize → HTTPS POST → WBXML parse → status classification) against a
/// real server with real credentials.
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn options_provision_foldersync_smoke() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (EAS_LIVE_URL/USER/PASSWORD) — skipping");
        return;
    };

    // 1. OPTIONS: the server advertises its protocol versions; negotiate ours.
    let probe = live_client(config.clone());
    let server = probe.options().await.expect("OPTIONS round-trip failed");
    assert!(
        !server.protocol_versions.is_empty(),
        "server advertised no MS-ASProtocolVersions"
    );
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common EAS protocol version with the server");
    config.protocol_version = negotiated;

    // 2. Provision: two-phase handshake; a permanent policy key must result. A server requesting
    //    RemoteWipe surfaces as an error here — we never auto-execute one.
    let mut client = live_client(config);
    client
        .provision()
        .await
        .expect("Provision handshake failed");
    assert!(
        !client.policy_key().is_empty(),
        "Provision completed without a permanent policy key"
    );

    // 3. FolderSync bootstrap (sync key "0"): the account must report at least one folder (every
    //    mailbox has an Inbox).
    let folders = client.folder_sync("0").await.expect("FolderSync failed");
    assert!(
        !folders.changes.is_empty(),
        "FolderSync bootstrap returned zero folders"
    );
}
