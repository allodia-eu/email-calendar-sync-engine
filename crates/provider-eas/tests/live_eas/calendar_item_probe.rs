// SPDX-License-Identifier: MPL-2.0
//! Live ItemOperations calendar fetches (location wire probe).

use super::*;

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

    // 4th-variant base: organizer fields present (the engine's Replace path
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

    let Some(mut config) = live_config() else {
        eprintln!("live gates unset (EAS_LIVE_URL/USER/PASSWORD) — skipping");
        return;
    };
    config.device_id = "KYLINSLIVETEST04".to_string();
    let probe = live_client(config.clone());
    let server = probe.options().await.expect("OPTIONS round-trip failed");
    let negotiated =
        pick_protocol_version(&server.protocol_versions.join(","), CLIENT_KNOWN_VERSIONS)
            .expect("no common EAS protocol version");
    config.protocol_version = negotiated.clone();
    let mut client = live_client(config);
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
        .filter(|f| matches!(f.folder_type, Some(8 | 13)))
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
                p.organizer_email = Some("felixzhou@example.org".to_string());
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
                        if let Some(ack) = outcome.add_acks.first()
                            && let Some(sid) = ack.server_id.clone()
                        {
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
                                    eprintln!("PROBE {folder_label} {label}: cleaned up {sid}");
                                }
                                Err(e) => {
                                    eprintln!("PROBE {folder_label} {label}: cleanup FAILED {e}");
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
