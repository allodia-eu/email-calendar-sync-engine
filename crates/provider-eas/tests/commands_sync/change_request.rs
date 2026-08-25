// SPDX-License-Identifier: MPL-2.0
//! Sync Change requests: spec shape, golden wire bytes, star/read flags.

use super::*;

#[test]
fn sync_change_request_uses_spec_shape() {
    let changes = vec![
        EasChange {
            server_id: "5:12".into(),
            read: Some(true),
            starred: None,
        },
        EasChange {
            server_id: "5:13".into(),
            read: Some(false),
            starred: None,
        },
    ];
    let tree = build_sync_change_request("5", "3", &changes);
    assert_eq!((tree.page, tree.token), (PAGE_AIRSYNC, AS_SYNC));
    assert_eq!(tree.children.len(), 1);
    let collections = &tree.children[0];
    assert_eq!(
        (collections.page, collections.token),
        (PAGE_AIRSYNC, AS_COLLECTIONS)
    );
    assert_eq!(collections.children.len(), 1);
    let collection = &collections.children[0];
    assert_eq!(
        (collection.page, collection.token),
        (PAGE_AIRSYNC, AS_COLLECTION)
    );

    // Collection children: SyncKey, CollectionId, Commands — nothing else.
    assert_eq!(collection.children.len(), 3);
    let sync_key = &collection.children[0];
    assert_eq!((sync_key.page, sync_key.token), (PAGE_AIRSYNC, AS_SYNC_KEY));
    assert_eq!(text_value(sync_key).unwrap(), "3");
    let cid = &collection.children[1];
    assert_eq!((cid.page, cid.token), (PAGE_AIRSYNC, AS_COLLECTION_ID));
    assert_eq!(text_value(cid).unwrap(), "5");
    assert!(
        collection.children.iter().all(
            |c| !(c.page == PAGE_AIRSYNC && (c.token == AS_CLASS || c.token == AS_GET_CHANGES))
        ),
        "upsync Collection must not carry Class or GetChanges"
    );

    let commands = &collection.children[2];
    assert_eq!((commands.page, commands.token), (PAGE_AIRSYNC, AS_COMMANDS));
    assert_eq!(commands.children.len(), 2);
    for (i, (sid, read_str)) in [("5:12", "1"), ("5:13", "0")].iter().enumerate() {
        let change = &commands.children[i];
        assert_eq!((change.page, change.token), (PAGE_AIRSYNC, AS_CHANGE));
        assert_eq!(change.children.len(), 2);
        let server_id = &change.children[0];
        assert_eq!(
            (server_id.page, server_id.token),
            (PAGE_AIRSYNC, AS_SERVER_ID)
        );
        assert_eq!(text_value(server_id).unwrap(), *sid);
        let app_data = &change.children[1];
        assert_eq!(
            (app_data.page, app_data.token),
            (PAGE_AIRSYNC, AS_APPLICATION_DATA)
        );
        assert_eq!(app_data.children.len(), 1);
        let read = &app_data.children[0];
        // Read is the Email-page (2) token 0x15 per MS-ASWBXML §2.1.2.1.3.
        assert_eq!(
            (read.page, read.token),
            (tags::email::PAGE, tags::email::READ)
        );
        assert_eq!(text_value(read).unwrap(), *read_str);
    }
}

/// Golden-bytes test: the serialized upsync request must match this exact
/// vector (hand-derived from the WBXML spec: AirSync page 0 is the initial
/// page, the only page switch is into Email page 2 for `<Read>`).
#[test]
fn sync_change_request_matches_golden_wire_bytes() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: Some(true),
        starred: None,
    }];
    let tree = build_sync_change_request("5", "3", &changes);
    let bytes = provider_eas::wbxml::serialize_tree(&tree).expect("serialize");
    let expected: &[u8] = &[
        0x03, 0x01, 0x6A, 0x00, // WBXML header
        0x45, // Sync (0x05|0x40) on page 0 (initial page, no switch)
        0x5C, // Collections (0x1C|0x40)
        0x4F, // Collection (0x0F|0x40)
        0x4B, 0x03, 0x33, 0x00, 0x01, // SyncKey STR_I "3" + END
        0x52, 0x03, 0x35, 0x00, 0x01, // CollectionId STR_I "5" + END
        0x56, // Commands (0x16|0x40)
        0x48, // Change (0x08|0x40)
        0x4D, 0x03, 0x35, 0x3A, 0x31, 0x32, 0x00, 0x01, // ServerId STR_I "5:12" + END
        0x5D, // ApplicationData (0x1D|0x40)
        0x00, 0x02, // SWITCH_PAGE 2 (Email)
        0x55, 0x03, 0x31, 0x00, 0x01, // Read STR_I "1" + END
        0x01, // END ApplicationData
        0x01, // END Change
        0x01, // END Commands
        0x01, // END Collection
        0x01, // END Collections
        0x01, // END Sync
    ];
    assert_eq!(
        bytes, expected,
        "upsync request bytes drifted from the golden vector"
    );
}

/// The request tree survives a serialize/deserialize round trip.
#[test]
fn sync_change_request_round_trips() {
    let changes = vec![EasChange {
        server_id: "7:99".into(),
        read: Some(false),
        starred: None,
    }];
    let tree = build_sync_change_request("7", "key-1", &changes);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Task 3's builder emits only `read` — a starred-only change still
/// produces a schema-valid Change (ServerId + ApplicationData), with no
/// Read element inside ApplicationData. Task 4 extends the builder to
/// emit the Flag element for `starred`.
#[test]
fn sync_change_request_starred_only_change_has_no_read_element() {
    let changes = vec![EasChange {
        server_id: "5:1".into(),
        read: None,
        starred: Some(true),
    }];
    let tree = build_sync_change_request("5", "3", &changes);
    let change = &tree.children[0].children[0].children[2].children[0];
    assert_eq!((change.page, change.token), (PAGE_AIRSYNC, AS_CHANGE));
    let app_data = &change.children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert!(
        app_data
            .children
            .iter()
            .all(|c| !(c.page == tags::email::PAGE && c.token == tags::email::READ)),
        "no Read element when change.read is None"
    );
}

/// Task 4 golden-bytes: `starred: Some(true)` emits the full task-like
/// Flag container per Android EasSync.java:295-315 —
///   Flag (2,0x3A) > Status (2,0x3B)="2" + FlagType (2,0x3D)="FollowUp"
///     + tasks:StartDate (9,0x1E) + tasks:UtcStartDate (9,0x1F)
///     + tasks:DueDate (9,0x0C) + tasks:UtcDueDate (9,0x0D)
/// with the code page switching email(2) → tasks(9) mid-container
/// ([MS-ASWBXML] §2.1.2.1.3 / §2.1.2.1.10). The clock is pinned via
/// `build_sync_change_request_at` so the vector is deterministic:
/// now = 2026-01-01T00:00:00.000Z (epoch millis 1_767_225_600_000),
/// due = now + 7 days = 2026-01-08T00:00:00.000Z.
#[test]
fn sync_change_request_star_set_matches_golden_wire_bytes() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: None,
        starred: Some(true),
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let bytes = provider_eas::wbxml::serialize_tree(&tree).expect("serialize");
    let start = b"2026-01-01T00:00:00.000Z";
    let due = b"2026-01-08T00:00:00.000Z";
    let mut expected: Vec<u8> = vec![
        0x03, 0x01, 0x6A, 0x00, // WBXML header
        0x45, // Sync (0x05|0x40) on page 0 (initial page, no switch)
        0x5C, // Collections (0x1C|0x40)
        0x4F, // Collection (0x0F|0x40)
        0x4B, 0x03, 0x33, 0x00, 0x01, // SyncKey STR_I "3" + END
        0x52, 0x03, 0x35, 0x00, 0x01, // CollectionId STR_I "5" + END
        0x56, // Commands (0x16|0x40)
        0x48, // Change (0x08|0x40)
        0x4D, 0x03, 0x35, 0x3A, 0x31, 0x32, 0x00, 0x01, // ServerId STR_I "5:12" + END
        0x5D, // ApplicationData (0x1D|0x40)
        0x00, 0x02, // SWITCH_PAGE 2 (Email)
        0x7A, // Flag (0x3A|0x40)
        0x7B, 0x03, 0x32, 0x00, 0x01, // Status (0x3B|0x40) STR_I "2" + END
        0x7D, 0x03, // FlagType (0x3D|0x40) STR_I
    ];
    expected.extend_from_slice(b"FollowUp");
    expected.extend_from_slice(&[0x00, 0x01]); // STR_I terminator + END FlagType
    expected.extend_from_slice(&[0x00, 0x09]); // SWITCH_PAGE 9 (Tasks)
    // StartDate (0x1E|0x40), UtcStartDate (0x1F|0x40),
    // DueDate (0x0C|0x40), UtcDueDate (0x0D|0x40)
    for (tag, val) in [(0x5Eu8, start), (0x5F, start), (0x4C, due), (0x4D, due)] {
        expected.push(tag);
        expected.push(0x03); // STR_I
        expected.extend_from_slice(val);
        expected.extend_from_slice(&[0x00, 0x01]); // STR_I terminator + END
    }
    // END Flag, ApplicationData, Change, Commands, Collection, Collections, Sync
    expected.extend_from_slice(&[0x01; 7]);
    assert_eq!(
        bytes, expected,
        "star-set upsync bytes drifted from the golden vector"
    );
}

/// Task 4 golden-bytes: `starred: Some(false)` emits an EMPTY `<Flag/>`
/// element (bare token 0x3A, no WITH_CONTENT bit, no children, no page
/// switch into Tasks) — Android's `s.tag(Tags.EMAIL_FLAG)` form.
#[test]
fn sync_change_request_star_clear_matches_golden_wire_bytes() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: None,
        starred: Some(false),
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let bytes = provider_eas::wbxml::serialize_tree(&tree).expect("serialize");
    let expected: &[u8] = &[
        0x03, 0x01, 0x6A, 0x00, // WBXML header
        0x45, // Sync
        0x5C, // Collections
        0x4F, // Collection
        0x4B, 0x03, 0x33, 0x00, 0x01, // SyncKey "3"
        0x52, 0x03, 0x35, 0x00, 0x01, // CollectionId "5"
        0x56, // Commands
        0x48, // Change
        0x4D, 0x03, 0x35, 0x3A, 0x31, 0x32, 0x00, 0x01, // ServerId "5:12"
        0x5D, // ApplicationData
        0x00, 0x02, // SWITCH_PAGE 2 (Email)
        0x3A, // empty <Flag/> — bare token, no WITH_CONTENT bit, no END of its own
        0x01, // END ApplicationData
        0x01, // END Change
        0x01, // END Commands
        0x01, // END Collection
        0x01, // END Collections
        0x01, // END Sync
    ];
    assert_eq!(
        bytes, expected,
        "star-clear upsync bytes drifted from the golden vector"
    );
}

/// Structural check for a combined read+star change: Read is emitted
/// before the Flag container (Android's order), and the Flag children
/// carry the spec tokens/pages with the pinned dates.
#[test]
fn sync_change_request_read_and_star_emit_read_before_flag() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: Some(true),
        starred: Some(true),
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let app_data = &tree.children[0].children[0].children[2].children[0].children[1];
    assert_eq!(
        (app_data.page, app_data.token),
        (PAGE_AIRSYNC, AS_APPLICATION_DATA)
    );
    assert_eq!(app_data.children.len(), 2);

    let read = &app_data.children[0];
    assert_eq!(
        (read.page, read.token),
        (tags::email::PAGE, tags::email::READ)
    );
    assert_eq!(text_value(read).unwrap(), "1");

    let flag = &app_data.children[1];
    assert_eq!(
        (flag.page, flag.token),
        (tags::email::PAGE, tags::email::FLAG)
    );
    assert_eq!(flag.children.len(), 6);
    // Status (email page 2, 0x3B) = "2"
    assert_eq!((flag.children[0].page, flag.children[0].token), (2, 0x3B));
    assert_eq!(text_value(&flag.children[0]).unwrap(), "2");
    // FlagType (email page 2, 0x3D) = "FollowUp"
    assert_eq!((flag.children[1].page, flag.children[1].token), (2, 0x3D));
    assert_eq!(text_value(&flag.children[1]).unwrap(), "FollowUp");
    // Tasks-page (9) dates: StartDate 0x1E, UtcStartDate 0x1F,
    // DueDate 0x0C, UtcDueDate 0x0D — start = now, due = now + 7 days.
    for (i, tok) in [0x1Eu8, 0x1F, 0x0C, 0x0D].iter().enumerate() {
        let el = &flag.children[2 + i];
        assert_eq!((el.page, el.token), (9, *tok));
    }
    assert_eq!(
        text_value(&flag.children[2]).unwrap(),
        "2026-01-01T00:00:00.000Z"
    );
    assert_eq!(
        text_value(&flag.children[3]).unwrap(),
        "2026-01-01T00:00:00.000Z"
    );
    assert_eq!(
        text_value(&flag.children[4]).unwrap(),
        "2026-01-08T00:00:00.000Z"
    );
    assert_eq!(
        text_value(&flag.children[5]).unwrap(),
        "2026-01-08T00:00:00.000Z"
    );
}

/// `starred: None` (with read present) emits no Flag element at all —
/// the pre-Task-4 ApplicationData shape is preserved.
#[test]
fn sync_change_request_starred_none_emits_no_flag_element() {
    let changes = vec![EasChange {
        server_id: "5:12".into(),
        read: Some(false),
        starred: None,
    }];
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_767_225_600_000);
    let tree = build_sync_change_request_at("5", "3", &changes, now);
    let app_data = &tree.children[0].children[0].children[2].children[0].children[1];
    assert_eq!(app_data.children.len(), 1);
    assert!(
        app_data
            .children
            .iter()
            .all(|c| !(c.page == tags::email::PAGE && c.token == tags::email::FLAG)),
        "no Flag element when change.starred is None"
    );
}

/// The std-only EAS UTC formatter: epoch, a round-midnight instant (the
/// golden-vector clock), and a leap-day + sub-second boundary.
#[test]
fn eas_datetime_utc_formats_epoch_round_midnight_and_leap_day() {
    use std::time::{Duration, UNIX_EPOCH};
    assert_eq!(
        format_eas_datetime_utc(UNIX_EPOCH),
        "1970-01-01T00:00:00.000Z"
    );
    assert_eq!(
        format_eas_datetime_utc(UNIX_EPOCH + Duration::from_millis(1_767_225_600_000)),
        "2026-01-01T00:00:00.000Z"
    );
    // 2024-02-29T23:59:59.999Z — leap day, end-of-day, max millis.
    assert_eq!(
        format_eas_datetime_utc(UNIX_EPOCH + Duration::from_millis(1_709_251_199_999)),
        "2024-02-29T23:59:59.999Z"
    );
}
