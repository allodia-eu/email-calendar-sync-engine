// SPDX-License-Identifier: MPL-2.0
//! Live FolderCreate/FolderDelete/FolderSync probes against the calendar hierarchy.

use super::*;

/// Step-5 delta variant (2026-08-22 outstanding item): prove a NEWLY
/// created type-13 calendar folder arrives via a WARM-key FolderSync DELTA
/// (not the per-process key-"0" bootstrap). The create must come from a
/// DIFFERENT device (client B): a self-create rotates our own hierarchy key
/// via the FolderCreate response (the create is baked into the new key), so
/// our warm round would see nothing — the production delta case is another
/// client (OWA / the user) creating while OUR engine holds a warm key.
/// Leaves "Cal-13-Delta" on the server as the Step-5 artifact; cleans up
/// any earlier probe leftovers of the same name first.
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn calendar_folder_create_delta_probe() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset — skipping");
        return;
    };
    config.device_id = "KYLINSLIVETEST05".to_string();
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common version");
    config.protocol_version = negotiated;

    // Client A (observer): bootstrap the hierarchy, hold the warm key.
    let mut client_a = EasClient::new(config.clone());
    client_a.provision().await.expect("Provision A failed");
    let boot = client_a
        .folder_sync("0")
        .await
        .expect("bootstrap FolderSync");

    // Clean up earlier probe leftovers of the same name (best-effort).
    for stale in boot
        .changes
        .iter()
        .filter(|f| f.display_name == "Cal-13-Delta")
    {
        let mut janitor = EasClient::new(config.clone());
        janitor.provision().await.expect("Provision janitor failed");
        let _ = janitor
            .folder_delete(&provider_eas::types::FolderDeleteRequest {
                server_id: stale.server_id.clone(),
            })
            .await;
        eprintln!("DELTA-PROBE cleaned stale {}", stale.server_id);
    }
    if boot
        .changes
        .iter()
        .any(|f| f.display_name == "Cal-13-Delta")
    {
        // Re-bootstrap after cleanup so the warm key reflects the clean state.
        let boot = client_a.folder_sync("0").await.expect("re-bootstrap");
        eprintln!("DELTA-PROBE re-bootstrapped after cleanup");
        let _ = boot;
    }

    // Parent: the type-8 default calendar (natural home for a user calendar).
    let parent = boot
        .changes
        .iter()
        .find(|f| f.folder_type == Some(8))
        .expect("no type-8 calendar folder");
    let warm_key = client_a.hierarchy_key().to_string();
    eprintln!(
        "DELTA-PROBE parent {} (type {:?}, {:?}), warm key {}",
        parent.server_id, parent.folder_type, parent.display_name, warm_key
    );

    // Warm-key round BEFORE the create: must be an empty delta.
    let empty = client_a.folder_sync(&warm_key).await.expect("warm round 1");
    assert!(
        empty.changes.is_empty(),
        "expected empty delta before create, got {:?}",
        empty.changes
    );

    // Client B (a DIFFERENT device id) creates the folder — its FolderCreate
    // rotates B's key, not A's, so A's warm key must learn it via delta.
    let mut config_b = config.clone();
    config_b.device_id = "KYLINSLIVETEST06".to_string();
    let mut client_b = EasClient::new(config_b);
    client_b.provision().await.expect("Provision B failed");
    let _ = client_b.folder_sync("0").await.expect("B bootstrap");
    let (fc_status, fc_sid) = client_b
        .folder_create(&provider_eas::types::FolderCreateRequest {
            parent_id: parent.server_id.clone(),
            display_name: "Cal-13-Delta".to_string(),
            class: "Calendar".to_string(),
        })
        .await
        .expect("FolderCreate failed");
    eprintln!("DELTA-PROBE B created: status {fc_status}, server_id {fc_sid:?}");

    // THE DELTA PROOF: A's (stale) warm key now returns exactly the new
    // folder as a type-13 Add.
    let warm_now = client_a.hierarchy_key().to_string();
    let delta = client_a
        .folder_sync(&warm_now)
        .await
        .expect("warm round 2 after B's create");
    let adds: Vec<_> = delta
        .changes
        .iter()
        .map(|f| (f.server_id.as_str(), f.folder_type, f.display_name.as_str()))
        .collect();
    eprintln!("DELTA-PROBE delta round changes: {adds:?}");
    assert_eq!(delta.changes.len(), 1, "exactly one Add expected");
    assert_eq!(
        delta.changes[0].folder_type,
        Some(13),
        "type 13 in the delta"
    );
    assert_eq!(delta.changes[0].display_name, "Cal-13-Delta");
}

/// Delete-delta proof (per-device ServerId spaces!): B creates a uniquely
/// named folder; A's warm key sees the Add (A-space id); B deletes it; A's
/// NEXT warm key must report a Delete carrying EXACTLY the A-space id from
/// A's own Add — comparing ids across devices is meaningless (live evidence
/// 2026-08-22: calendar-2 = id 53 for the engine device, id 8 for another).
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn calendar_folder_delete_delta_probe() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset — skipping");
        return;
    };
    config.device_id = "KYLINSLIVETEST05".to_string();
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common version");
    config.protocol_version = negotiated;

    let mut client_a = EasClient::new(config.clone());
    client_a.provision().await.expect("Provision A failed");
    let boot = client_a.folder_sync("0").await.expect("bootstrap");
    let parent = boot
        .changes
        .iter()
        .find(|f| f.folder_type == Some(8))
        .expect("no type-8 calendar");
    let name = format!("Cal-DelProbe-{}", std::process::id() % 10000);
    let warm0 = client_a.hierarchy_key().to_string();
    let empty = client_a.folder_sync(&warm0).await.expect("warm 1");
    assert!(empty.changes.is_empty(), "pre-create delta not empty");

    // B (different device) creates the probe folder.
    let mut config_b = config.clone();
    config_b.device_id = "KYLINSLIVETEST06".to_string();
    let mut client_b = EasClient::new(config_b);
    client_b.provision().await.expect("Provision B failed");
    let _ = client_b.folder_sync("0").await;
    client_b
        .folder_create(&provider_eas::types::FolderCreateRequest {
            parent_id: parent.server_id.clone(),
            display_name: name.clone(),
            class: "Calendar".to_string(),
        })
        .await
        .expect("B FolderCreate failed");

    // A sees the Add with its OWN id for the folder.
    let w = client_a.hierarchy_key().to_string();
    let add_delta = client_a.folder_sync(&w).await.expect("warm 2 (add)");
    let added = add_delta
        .changes
        .iter()
        .find(|f| f.display_name == name)
        .expect("A did not see the Add delta");
    let a_space_id = added.server_id.clone();
    eprintln!("DEL-PROBE A saw Add {a_space_id} ({name})");

    // B deletes the folder (B-space id from B's own bootstrap).
    let boot_b = client_b.folder_sync("0").await.expect("B re-bootstrap");
    let b_folder = boot_b
        .changes
        .iter()
        .find(|f| f.display_name == name)
        .expect("B cannot see the folder");
    client_b
        .folder_delete(&provider_eas::types::FolderDeleteRequest {
            server_id: b_folder.server_id.clone(),
        })
        .await
        .expect("B FolderDelete failed");

    // A's next warm delta must Delete exactly the A-space id.
    let w = client_a.hierarchy_key().to_string();
    let del_delta = client_a.folder_sync(&w).await.expect("warm 3 (delete)");
    eprintln!(
        "DEL-PROBE delta deletions {:?} (expect [{a_space_id}])",
        del_delta.deletions
    );
    assert_eq!(del_delta.deletions, vec![a_space_id.clone()]);
}

#[tokio::test]
#[ignore = "live Exchange account required"]
async fn calendar_folder_truth_probe() {
    let Some(mut config) = live_config() else {
        return;
    };
    config.device_id = "KYLINSLIVETEST07".to_string();
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS");
    config.protocol_version =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS).unwrap();
    let mut c = EasClient::new(config);
    c.provision().await.expect("provision");
    let boot = c.folder_sync("0").await.expect("fs");
    let names: Vec<_> = boot
        .changes
        .iter()
        .map(|f| (f.server_id.as_str(), f.folder_type, f.display_name.as_str()))
        .collect();
    eprintln!(
        "TRUTH total {} folders; cal-related: {:?}",
        names.len(),
        names
            .iter()
            .filter(|(_, t, _)| matches!(t, Some(8 | 13)))
            .collect::<Vec<_>>()
    );
}

/// Cleanup: delete the drill's Cal-13-Delta folders (per-device ServerId
/// spaces — this probe bootstraps its OWN map and deletes by name). EAS
/// FolderSync ServerIds are PER-DEVICE-PARTNERSHIP: the same mailbox folder
/// has different ids for different DeviceIds (live evidence 2026-08-22:
/// calendar-2 was id 53 for the engine device, 8 for KYLINSLIVETEST07).
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn calendar_folder_drill_cleanup() {
    let Some(mut config) = live_config() else {
        return;
    };
    config.device_id = "KYLINSLIVETEST07".to_string();
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS");
    config.protocol_version =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS).unwrap();
    let mut c = EasClient::new(config);
    c.provision().await.expect("provision");
    let boot = c.folder_sync("0").await.expect("fs");
    for f in boot
        .changes
        .iter()
        .filter(|f| f.display_name == "Cal-13-Delta")
    {
        let (st, _) = c
            .folder_delete(&provider_eas::types::FolderDeleteRequest {
                server_id: f.server_id.clone(),
            })
            .await
            .expect("delete");
        eprintln!(
            "CLEANUP deleted {} ({}): status {st}",
            f.server_id, f.display_name
        );
    }
    let after = c.folder_sync("0").await.expect("fs after");
    let left: Vec<_> = after
        .changes
        .iter()
        .filter(|f| matches!(f.folder_type, Some(8 | 13)))
        .map(|f| (f.server_id.as_str(), f.display_name.as_str()))
        .collect();
    eprintln!("CLEANUP remaining calendar folders: {left:?}");
    assert!(
        !left.iter().any(|(_, n)| *n == "Cal-13-Delta"),
        "all drill folders gone"
    );
}
