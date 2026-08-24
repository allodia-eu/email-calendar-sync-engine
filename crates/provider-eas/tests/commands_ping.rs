// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::{tests_common::*, *};

#[test]
fn ping_request_round_trips() {
    let req = PingRequest {
        heartbeat_interval: 60,
        monitored_collections: vec![PingCollection {
            collection_id: "col-1".to_string(),
            class: "Email".to_string(),
        }],
    };
    let tree = build_ping_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// MS-ASCMD 2.2.3.177.11 (and mailkit_arkts PingStatus): status 1 =
/// "heartbeat interval expired before any changes occurred" — NO
/// changes. This was historically inverted in our mapping.
#[test]
fn ping_response_status_1_is_expired_no_changes() {
    let response = WbxmlElement::container(
        PAGE_PING,
        PING_PING,
        vec![WbxmlElement::text(PAGE_PING, PING_STATUS, "1")],
    );
    let parsed = parse_ping_response(&response).expect("parse");
    assert_eq!(parsed.status, "Expired");
    assert!(parsed.folders.is_empty());
}

/// Status 2 = "changes occurred in at least one monitored folder; the
/// response specifies the changed folders" (MS-ASCMD 2.2.3.177.11).
#[test]
fn ping_response_status_2_is_changes() {
    let response = WbxmlElement::container(
        PAGE_PING,
        PING_PING,
        vec![WbxmlElement::text(PAGE_PING, PING_STATUS, "2")],
    );
    let parsed = parse_ping_response(&response).expect("parse");
    assert_eq!(parsed.status, "Changes");
    assert!(parsed.folders.is_empty());
}

/// Live wire evidence (dev.cmmp.hksarg, 2026-08-03/04): a flag change in
/// OWA is answered at the hold boundary with `<Status>2</Status>` AND a
/// `<Folders>` list naming the changed collections — exactly the
/// spec's status-2 shape. The parser must surface those folder ids.
#[test]
fn ping_response_collects_changed_folder_ids_under_status_2() {
    let response = WbxmlElement::container(
        PAGE_PING,
        PING_PING,
        vec![
            WbxmlElement::text(PAGE_PING, PING_STATUS, "2"),
            WbxmlElement::container(
                PAGE_PING,
                PING_FOLDERS,
                vec![
                    WbxmlElement::text(PAGE_PING, PING_FOLDER, "5"),
                    WbxmlElement::text(PAGE_PING, PING_FOLDER, "6"),
                    WbxmlElement::text(PAGE_PING, PING_FOLDER, "11"),
                ],
            ),
        ],
    );
    let parsed = parse_ping_response(&response).expect("parse");
    assert_eq!(parsed.status, "Changes");
    assert_eq!(parsed.folders, vec!["5", "6", "11"]);
}

#[test]
fn parse_ping_response_reads_heartbeat_interval() {
    let tree = WbxmlElement::container(
        PAGE_PING,
        PING_PING,
        vec![
            WbxmlElement::text(PAGE_PING, PING_STATUS, "5"),
            WbxmlElement::text(PAGE_PING, PING_HEARTBEAT_INTERVAL, "60"),
        ],
    );
    let r = parse_ping_response(&tree).expect("parse");
    assert_eq!(r.status, "5");
    assert_eq!(r.heartbeat_interval, Some(60));
}

#[test]
fn parse_ping_response_no_interval_defaults_none() {
    let tree = WbxmlElement::container(
        PAGE_PING,
        PING_PING,
        vec![WbxmlElement::text(PAGE_PING, PING_STATUS, "2")],
    );
    let r = parse_ping_response(&tree).expect("parse");
    assert_eq!(r.heartbeat_interval, None);
}
