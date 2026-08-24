// SPDX-License-Identifier: MPL-2.0
//! Gated live tests against a real Exchange/O365 account.
//!
//! These tests are `#[ignore]`d by default and additionally no-op when the
//! environment gates are unset, so a plain `cargo test -p provider-eas` never
//! touches the network. To run them for real:
//!
//! ```sh
//! # KYLINS_EAS_LIVE_USERNAME — optional: Basic-auth identity when it differs
//! # from the mailbox address (EAS `User` param); unset → identity = USER.
//! # KYLINS_EAS_LIVE_INSECURE — optional: set to 1 to accept self-signed certs.
//! KYLINS_EAS_LIVE_URL=https://mail.example.com/Microsoft-Server-ActiveSync \
//! KYLINS_EAS_LIVE_USER=user@example.com \
//! KYLINS_EAS_LIVE_PASS=app-password \
//! KYLINS_EAS_LIVE_USERNAME=user@example.local \
//! KYLINS_EAS_LIVE_INSECURE=1 \
//! cargo test -p provider-eas --test live_eas -- --include-ignored --nocapture
//! ```
//!
//! The scaffold uses the Basic-auth path (`EasConfig { username, password, ..
//! }` with `auth: None`). OAuth accounts need a token provider wired through
//! `types::EasConfig::auth` and are out of scope for this smoke test.

use provider_eas::{
    client::{EasClient, pick_protocol_version},
    types::{EasConfig, SyncRequest},
};

/// Protocol versions this client knows how to speak, mirroring the account
/// setup flow in the host app (kylins `api::eas`).
const CLIENT_KNOWN_VERSIONS: &[&str] = &["2.5", "12.0", "12.1", "14.0", "14.1", "16.0", "16.1"];

/// Build an `EasConfig` from the environment gates, or `None` when any gate
/// is unset (the test then returns immediately — a skip, not a pass/fail).
fn live_config() -> Option<EasConfig> {
    let url = std::env::var("KYLINS_EAS_LIVE_URL").ok()?;
    let user = std::env::var("KYLINS_EAS_LIVE_USER").ok()?;
    let pass = std::env::var("KYLINS_EAS_LIVE_PASS").ok()?;
    // Basic-auth identity. Distinct from the EAS `User` param when the
    // server's auth realm differs from the mailbox address (the on-prem
    // test server: auth felixzhou@kylins.local, User felixzhou@kylins.com).
    // Unset → identity equals User (the common case).
    let username = std::env::var("KYLINS_EAS_LIVE_USERNAME").unwrap_or_else(|_| user.clone());
    // Self-signed test servers (e.g. the on-prem Exchange at
    // mail.kylins.com): opt in explicitly — default stays secure.
    let insecure = std::env::var("KYLINS_EAS_LIVE_INSECURE").is_ok();
    Some(EasConfig {
        url,
        username,
        user,
        password: pass,
        // Alphanumeric, <= 16 chars per MS-ASHTTP DeviceId constraints.
        device_id: "KYLINSLIVETEST01".to_string(),
        accept_invalid_certs: insecure,
        ..Default::default()
    })
}

/// Smoke: OPTIONS version negotiation → two-phase Provision handshake →
/// bootstrap FolderSync. Exercises the whole transport stack (WBXML
/// serialize → HTTPS POST → WBXML parse → status classification) against a
/// real server with real credentials.
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn options_provision_foldersync_smoke() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (KYLINS_EAS_LIVE_URL/USER/PASS) — skipping");
        return;
    };

    // 1. OPTIONS: the server advertises its protocol versions; negotiate ours.
    let probe = EasClient::new(config.clone());
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
    let mut client = EasClient::new(config);
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
    // (observed on mail.kylins.com 2026-08-16: phase-1 status 135, "sync
    // state already exists"; serialized runs pass cleanly). Two DIFFERENT
    // devices provisioning the same mailbox concurrently is a normal
    // server scenario, so this keeps the stock parallel invocation safe.
    config.device_id = "KYLINSLIVETEST02".to_string();

    // Negotiate + provision exactly like the existing smoke test.
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS round-trip failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common EAS protocol version with the server");
    config.protocol_version = negotiated;
    let mut client = EasClient::new(config);
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
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS round-trip failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common EAS protocol version with the server");
    config.protocol_version = negotiated;
    let mut client = EasClient::new(config);
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

/// Smoke: find a Contacts folder (folder_type 9) in the FolderSync bootstrap
/// and Sync one page of it with class "Contacts" — proves the MS-ASCNTC parse
/// path (the M8-C task-1 seam: `SyncResult::contacts_added`) against a real
/// server. Cheap window, no bodies needed. An EMPTY contacts folder still
/// passes: transport/status/key-advance is the proof here, item-level
/// parsing is covered by the unit goldens in contacts.rs.
///
/// Wire expectations cited: folder Type 9 = Contacts per [MS-ASFD]
/// `FolderHierarchy:Type` (see the `EasFolder.folder_type` doc comment in
/// crates/provider-eas/src/types.rs); the collection's items arrive as
/// Contacts-class ApplicationData per [MS-ASCNTC] §2.2 (FileAs/FirstName/
/// Email1Address are §2.2.2.30/§2.2.2.31/§2.2.2.27 — see the
/// `ContactsContactProps` field docs in contacts.rs).
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn contacts_first_page_smoke() {
    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (KYLINS_EAS_LIVE_URL/USER/PASS) — skipping");
        return;
    };

    // Own device id — the device-id-race lesson from sync_first_page_smoke:
    // the live tests run on parallel threads, and concurrent Provision
    // phase-1 handshakes from the SAME device identity race server-side
    // (status 135). Fourth live test → fourth device id.
    config.device_id = "KYLINSLIVETEST04".to_string();

    // Negotiate + provision exactly like the existing smoke tests.
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS round-trip failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common EAS protocol version with the server");
    config.protocol_version = negotiated;
    let mut client = EasClient::new(config);
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

/// Drill probe (2026-08-22): which Location wire form does the live server
/// accept inside a Calendar Sync Add? Live evidence said the legacy
/// calendar-page leaf (4, 0x17) is rejected with per-item Status 6
/// (conversion error) — this probe A/B/C-tests leaf vs airsyncbase
/// container vs absent, printing each per-item ack. Items land in 2030 and
/// are deleted immediately on success.
#[tokio::test]
#[ignore = "live Exchange account required"]
async fn calendar_location_wire_probe() {
    use provider_eas::{
        calendar_write::{
            CalendarEventWrite, build_calendar_application_data, build_fixed_offset_tzi_base64,
        },
        commands::{
            CalendarChange, build_calendar_change_request, new_calendar_client_id,
            parse_sync_change_response,
        },
    };

    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (KYLINS_EAS_LIVE_URL/USER/PASS) — skipping");
        return;
    };
    config.device_id = "KYLINSLIVETEST04".to_string();
    let probe = EasClient::new(config.clone());
    let server = probe.options().await.expect("OPTIONS round-trip failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common EAS protocol version");
    config.protocol_version = negotiated.clone();
    let mut client = EasClient::new(config);
    client
        .provision()
        .await
        .expect("Provision handshake failed");
    let folders = client.folder_sync("0").await.expect("FolderSync failed");
    // Every calendar folder (type 8 default + type 13 user-created) — the
    // 2026-08-22 engine drill saw the Add succeed on the default folder but
    // fail on the user-created one, so the probe must cover both.
    let calendars: Vec<_> = folders
        .changes
        .iter()
        .filter(|f| matches!(f.folder_type, Some(8) | Some(13)))
        .collect();
    eprintln!(
        "PROBE negotiated {negotiated}, calendar folders {:?}",
        calendars
            .iter()
            .map(|f| (f.server_id.as_str(), f.folder_type, f.display_name.as_str()))
            .collect::<Vec<_>>()
    );

    for calendar in &calendars {
        let folder_label = format!(
            "folder {} (type {:?}, {:?})",
            calendar.server_id, calendar.folder_type, calendar.display_name
        );
        // Bootstrap a real collection key (one GetChanges round on key "0").
        let req = SyncRequest {
            collection_id: calendar.server_id.clone(),
            sync_key: "0".to_string(),
            class: "Calendar".to_string(),
            window_size: 1,
            filter_age_days: 0,
            fetch_body: false,
            truncation_size: None,
            mime_support: None,
            mime_truncation: None,
            supported: None,
        };
        let page = client.sync(&req).await.expect("bootstrap Sync failed");
        assert_eq!(page.status, 1, "bootstrap collection status");
        let mut key = page.sync_key.clone();
        assert!(!key.is_empty() && key != "0", "key did not advance");

        // 4th variant: organizer fields present (the engine's Replace path
        // carries them from the downsynced row — suspected status-6 trigger).
        fn probe_props() -> CalendarEventWrite {
            CalendarEventWrite {
                start_time: "20300101T020000Z".to_string(),
                end_time: "20300101T030000Z".to_string(),
                all_day_event: false,
                time_zone_base64: build_fixed_offset_tzi_base64(480),
                ..Default::default()
            }
        }
        let mut variant_props: Vec<(&str, &str, CalendarEventWrite)> = vec![
            ("leaf-4-0x17", "14.1", {
                let mut p = probe_props();
                p.location = Some("Probe Room".to_string());
                p
            }),
            ("container-17-0x20", "16.1", {
                let mut p = probe_props();
                p.location = Some("Probe Room".to_string());
                p
            }),
            ("absent", "16.1", probe_props()),
            ("container-with-organizer", "16.1", {
                let mut p = probe_props();
                p.location = Some("Probe Room".to_string());
                p.organizer_email = Some("felixzhou@kylins.com".to_string());
                p.organizer_name = Some("Felixzhou User".to_string());
                p
            }),
        ];
        for (label, wire_version, mut props) in variant_props.drain(..) {
            props.subject = Some(format!("PROBE {label}"));
            // Sanity: the version param really selects the intended wire form.
            let app = build_calendar_application_data(&props, wire_version);
            eprintln!(
                "PROBE {folder_label} {label}: app_data pages/tokens {:?}",
                app.children
                    .iter()
                    .map(|c| (c.page, c.token))
                    .collect::<Vec<_>>()
            );
            let tree = build_calendar_change_request(
                &calendar.server_id,
                &key,
                &[CalendarChange::Add {
                    client_id: new_calendar_client_id(),
                    props,
                }],
                wire_version,
            );
            match client.send_command("Sync", &tree).await {
                Ok(resp) => match parse_sync_change_response(&resp) {
                    Ok(outcome) => {
                        eprintln!(
                            "PROBE {folder_label} {label}: collection status {}, acks {:?}",
                            outcome.status, outcome.add_acks
                        );
                        if let Some(ack) = outcome.add_acks.first() {
                            if let Some(sid) = ack.server_id.clone() {
                                let del = build_calendar_change_request(
                                    &calendar.server_id,
                                    &outcome.new_key,
                                    &[CalendarChange::Remove {
                                        server_id: sid.clone(),
                                    }],
                                    "16.1",
                                );
                                match client.send_command("Sync", &del).await {
                                    Ok(_) => {
                                        eprintln!("PROBE {folder_label} {label}: cleaned up {sid}")
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "PROBE {folder_label} {label}: cleanup FAILED {e}"
                                        )
                                    }
                                }
                            }
                        }
                        if outcome.status == 1 && !outcome.new_key.is_empty() {
                            key = outcome.new_key.clone();
                        }
                    }
                    Err(e) => eprintln!("PROBE {folder_label} {label}: PARSE ERROR {e}"),
                },
                Err(e) => eprintln!("PROBE {folder_label} {label}: SEND ERROR {e}"),
            }
        }
    }
}

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
    eprintln!("DELTA-PROBE delta round changes: {:?}", adds);
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
            .filter(|(_, t, _)| matches!(t, Some(8) | Some(13)))
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
        .filter(|f| matches!(f.folder_type, Some(8) | Some(13)))
        .map(|f| (f.server_id.as_str(), f.display_name.as_str()))
        .collect();
    eprintln!("CLEANUP remaining calendar folders: {left:?}");
    assert!(
        !left.iter().any(|(_, n)| *n == "Cal-13-Delta"),
        "all drill folders gone"
    );
}
