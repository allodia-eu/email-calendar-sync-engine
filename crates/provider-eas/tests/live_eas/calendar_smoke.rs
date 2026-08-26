// SPDX-License-Identifier: MPL-2.0
//! Live Sync first page against the calendar folder.

use super::*;

/// Smoke: find a Calendar folder (folder_type 8) in the FolderSync bootstrap
/// and Sync one page of it with class "Calendar" — proves the MS-ASCAL parse
/// path (the M8 Task-4 seam: `SyncResult::calendar_added`) against a real
/// server. Cheap window, no bodies needed.
///
/// Wire expectations cited: folder Type 8 = Calendar per [MS-ASFD]
/// `FolderHierarchy:Type` (see the `EasFolder.folder_type` doc comment in
/// crates/provider-eas/src/types.rs); the collection's items arrive as
/// Calendar-class ApplicationData per [MS-ASCAL] §2.2 (StartTime/DtStamp/
/// EndTime are the spec-required core, §2.2.2.42/§2.2.2.18/§2.2.2.20).
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn calendar_first_page_smoke() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (KYLINS_EAS_LIVE_URL/USER/PASS) — skipping");
        return;
    };

    // Own device id — the device-id-race lesson from sync_first_page_smoke:
    // the live tests run on parallel threads, and concurrent Provision
    // phase-1 handshakes from the SAME device identity race server-side
    // (status 135). Third live test → third device id.
    config.device_id = "KYLINSLIVETEST03".to_string();

    // Negotiate + provision exactly like the existing smoke tests.
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

    // Bootstrap hierarchy; enumerate the Calendar folders. Type 8 = Calendar
    // per [MS-ASFD] FolderHierarchy:Type.
    let folders = client.folder_sync("0").await.expect("FolderSync failed");
    let calendars: Vec<&_> = folders
        .changes
        .iter()
        .filter(|f| f.folder_type == Some(8))
        .collect();
    let names: Vec<&str> = calendars.iter().map(|f| f.display_name.as_str()).collect();
    eprintln!(
        "calendar inventory: {} folder(s) in bootstrap, {} calendar folder(s) (type 8): {:?}",
        folders.changes.len(),
        calendars.len(),
        names
    );
    let Some(calendar) = calendars.first() else {
        eprintln!(
            "test mailbox has NO calendar folder (folder_type 8) — skipping the \
             Calendar-class smoke (this is a skip, not a failure)"
        );
        return;
    };

    // First Sync page with class "Calendar". The request builder is
    // class-agnostic; the RESPONSE parser routes Add/Change items into
    // `calendar_added` / `calendar_updated` (M8 Task-4 seam). Tiny window
    // keeps the test cheap; fetch_body = false (header props only — the
    // smoke proves the parse path, not body delivery). `SyncRequest` has NO
    // Default derive — every field is listed explicitly.
    let req = SyncRequest {
        collection_id: calendar.server_id.clone(),
        sync_key: "0".to_string(),
        class: "Calendar".to_string(),
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
    // server must return a real (non-"0") key to continue.
    assert!(
        !page.sync_key.is_empty() && page.sync_key != "0",
        "Sync bootstrap did not advance the sync key: {:?}",
        page.sync_key
    );
    // Class-misrouting guard (fix r1): a Task-4 seam regression that lands
    // Calendar-class items in the email-shaped buckets would otherwise pass
    // silently (empty calendar_added skips the parse-sanity branch, and the
    // leak was only printed, never asserted). The parser routes by class and
    // `SyncResult`'s docs state the calendar vectors populate ONLY for class
    // "Calendar" — so the converse must hold here too.
    assert!(
        page.added.is_empty() && page.updated.is_empty(),
        "Calendar-class Sync leaked items into email-shaped buckets: added {} / updated {}",
        page.added.len(),
        page.updated.len()
    );
    eprintln!(
        "calendar first page: status {}, key advanced, calendar_added {} \
         calendar_updated {} deleted {} email-shaped added/updated {}/{} \
         more_available {}",
        page.status,
        page.calendar_added.len(),
        page.calendar_updated.len(),
        page.deleted_server_ids.len(),
        page.added.len(),
        page.updated.len(),
        page.more_available
    );

    // Parse sanity (tolerant): a calendar folder may legitimately hold items
    // missing some fields, but a first item with NOTHING parsed would mean
    // the Task-4 seam produced an empty ApplicationData — so at least one of
    // Subject/StartTime/Location must be present, else fail with the item's
    // debug dump.
    if let Some(first) = page.calendar_added.first() {
        assert!(
            first.props.subject.is_some()
                || first.props.start_time.is_some()
                || first.props.location.is_some(),
            "first calendar item parsed nothing observable: {first:#?}"
        );
        eprintln!("first calendar item: {first:#?}");
    }

    // Second round with the returned key: must succeed (status 1) even with
    // an empty change set — the held-key shape.
    let req2 = SyncRequest {
        sync_key: page.sync_key.clone(),
        ..req.clone()
    };
    let page2 = client.sync(&req2).await.expect("Sync round 2 failed");
    assert_eq!(page2.status, 1, "Sync round-2 status must be 1 (success)");
}
