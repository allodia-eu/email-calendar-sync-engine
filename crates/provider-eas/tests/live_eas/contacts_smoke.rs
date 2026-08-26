// SPDX-License-Identifier: MPL-2.0
//! Live Sync first page against the contacts folder.

use super::*;

/// Smoke: find a Contacts folder (folder_type 9) in the FolderSync bootstrap
/// and Sync one page of it with class "Contacts" — proves the MS-ASCNTC parse
/// path (the M8-C task-1 seam: `SyncResult::contacts_added`) against a real
/// server. Cheap window, no bodies needed. An EMPTY contacts folder still
/// passes: transport/status/key-advance is the proof here, item-level
/// parsing is covered by the unit goldens in `src/contacts/tests.rs`.
///
/// Wire expectations cited: folder Type 9 = Contacts per [MS-ASFD]
/// `FolderHierarchy:Type` (see the `EasFolder.folder_type` doc comment in
/// crates/provider-eas/src/types/folder.rs); the collection's items arrive as
/// Contacts-class ApplicationData per [MS-ASCNTC] §2.2 (FileAs/FirstName/
/// Email1Address are §2.2.2.30/§2.2.2.31/§2.2.2.27 — see the
/// `ContactsContactProps` field docs in `contacts/model.rs`).
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn contacts_first_page_smoke() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (EAS_LIVE_URL/USER/PASSWORD) — skipping");
        return;
    };

    // Own device id — the device-id-race lesson from sync_first_page_smoke:
    // the live tests run on parallel threads, and concurrent Provision
    // phase-1 handshakes from the SAME device identity race server-side
    // (status 135). Fourth live test → fourth device id.
    config.device_id = "KYLINSLIVETEST04".to_string();

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

    // Bootstrap hierarchy; enumerate the Contacts folders. Type 9 = Contacts
    // per [MS-ASFD] FolderHierarchy:Type.
    let folders = client.folder_sync("0").await.expect("FolderSync failed");
    let contacts: Vec<&_> = folders
        .changes
        .iter()
        .filter(|f| f.folder_type == Some(9))
        .collect();
    let names: Vec<&str> = contacts.iter().map(|f| f.display_name.as_str()).collect();
    eprintln!(
        "contacts inventory: {} folder(s) in bootstrap, {} contacts folder(s) (type 9): {:?}",
        folders.changes.len(),
        contacts.len(),
        names
    );
    let Some(contacts_folder) = contacts.first() else {
        // Skip-not-fail: report what folder types the account actually has
        // so the operator can see why the smoke skipped.
        let types: Vec<(Option<u8>, &str)> = folders
            .changes
            .iter()
            .map(|f| (f.folder_type, f.display_name.as_str()))
            .collect();
        eprintln!(
            "test mailbox has NO contacts folder (folder_type 9) — skipping the \
             Contacts-class smoke (this is a skip, not a failure). Folder \
             inventory (type, name): {types:?}"
        );
        return;
    };

    // First Sync page with class "Contacts". The request builder is
    // class-agnostic; the RESPONSE parser routes Add/Change items into
    // `contacts_added` / `contacts_updated` (M8-C task-1 seam). Tiny window
    // keeps the test cheap; fetch_body = false (the smoke proves the parse
    // path, not body delivery). `SyncRequest` has NO Default derive — every
    // field is listed explicitly.
    let req = SyncRequest {
        collection_id: contacts_folder.server_id.clone(),
        sync_key: "0".to_string(),
        class: "Contacts".to_string(),
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
    // Class-misrouting guard (the Task-12 calendar pattern): a seam
    // regression that lands Contacts-class items in the email-shaped buckets
    // would otherwise pass silently (empty contacts_added skips the
    // parse-sanity branch). The parser routes by class and `SyncResult`'s
    // docs state the contacts vectors populate ONLY for class "Contacts" —
    // so the converse must hold here too, and the calendar buckets must
    // equally stay empty (routing stayed exclusive).
    assert!(
        page.added.is_empty() && page.updated.is_empty(),
        "Contacts-class Sync leaked items into email-shaped buckets: added {} / updated {}",
        page.added.len(),
        page.updated.len()
    );
    assert!(
        page.calendar_added.is_empty() && page.calendar_updated.is_empty(),
        "Contacts-class Sync leaked items into calendar buckets: added {} / updated {}",
        page.calendar_added.len(),
        page.calendar_updated.len()
    );
    eprintln!(
        "contacts first page: status {}, key advanced, contacts_added {} \
         contacts_updated {} deleted {} email-shaped added/updated {}/{} \
         calendar-shaped added/updated {}/{} more_available {}",
        page.status,
        page.contacts_added.len(),
        page.contacts_updated.len(),
        page.deleted_server_ids.len(),
        page.added.len(),
        page.updated.len(),
        page.calendar_added.len(),
        page.calendar_updated.len(),
        page.more_available
    );

    // Parse sanity (tolerant): a contacts folder may legitimately hold items
    // missing some fields, but a first item with NOTHING parsed would mean
    // the task-1 seam produced an empty ApplicationData — so at least one of
    // FileAs/Email1Address/FirstName must be present, else fail with the
    // item's debug dump.
    if let Some(first) = page.contacts_added.first() {
        assert!(
            first.props.file_as.is_some()
                || first.props.email_1.is_some()
                || first.props.first_name.is_some(),
            "first contacts item parsed nothing observable: {first:#?}"
        );
        eprintln!("first contacts item: {first:#?}");
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
