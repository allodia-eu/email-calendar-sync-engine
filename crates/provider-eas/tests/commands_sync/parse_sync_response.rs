// SPDX-License-Identifier: MPL-2.0
//! parse_sync_response: Sync collection dispatch and status surfacing.

use super::*;

/// Full Sync-response fixture: Sync -> Collections -> Collection with
/// SyncKey="{sk1}", Status="1", MoreAvailable, and a Commands block
/// containing one Add (ServerId "1:1" + the email ApplicationData above).
///
/// Asserts the entire top-level orchestration path: sync_key, status,
/// more_available, and the added/updated/deleted vectors are populated by
/// walking the real tree through `parse_sync_response`.
#[test]
fn parse_sync_response_extracts_full_sync_collection() {
    let add_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "1:1"),
            fixture_email_app_data("Hello", "a@b", "c@d", "<p>hi</p>"),
        ],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![add_cmd]);
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{sk1}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE),
            commands,
        ],
    );
    let collections = WbxmlElement::container(PAGE_AIRSYNC, AS_COLLECTIONS, vec![collection]);
    let tree = WbxmlElement::container(PAGE_AIRSYNC, AS_SYNC, vec![collections]);

    let result = parse_sync_response(&tree).expect("parse_sync_response must succeed");

    // Top-level orchestration fields.
    assert_eq!(result.sync_key, "{sk1}");
    assert_eq!(
        result.status, 1,
        "success status must surface from Collection/Status"
    );
    assert!(
        result.more_available,
        "MoreAvailable element must set more_available=true"
    );

    // Added item: full envelope must round-trip through parse_item ->
    // parse_application_data (covered in depth by Task 2; here we lock the
    // Add-dispatch wiring at the Commands level).
    assert_eq!(result.added.len(), 1, "exactly one Add command");
    let added = &result.added[0];
    assert_eq!(added.server_id, "1:1");
    assert_eq!(added.subject.as_deref(), Some("Hello"));
    assert_eq!(added.from.as_deref(), Some("a@b"));
    assert_eq!(added.to.as_deref(), Some("c@d"));
    assert_eq!(
        added.body_html.as_deref(),
        Some("<p>hi</p>"),
        "Body Type=2 must populate body_html"
    );

    // No Change/Delete in this fixture.
    assert!(result.updated.is_empty(), "no Change commands in fixture");
    assert!(
        result.deleted_server_ids.is_empty(),
        "no Delete commands in fixture"
    );
}

/// A Commands block with Change + Delete must populate `updated` and
/// `deleted_server_ids` respectively, and leave `added` empty.
#[test]
fn parse_sync_response_dispatches_change_and_delete() {
    let change_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_CHANGE,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "2:2"),
            fixture_email_app_data("Updated", "x@y", "z@w", "<p>u</p>"),
        ],
    );
    // EAS Delete is a CONTAINER carrying the ServerId as a child element
    // (MS-ASCMD 2.2.3.42.2), the same shape Add/Change use.
    let delete_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_DELETE,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "3:3")],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![change_cmd, delete_cmd]);
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{sk2}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            commands,
        ],
    );
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert!(result.added.is_empty(), "no Add in this fixture");
    assert_eq!(result.updated.len(), 1, "one Change");
    assert_eq!(result.updated[0].server_id, "2:2");
    assert_eq!(
        result.deleted_server_ids,
        vec!["3:3".to_string()],
        "Delete ServerId must land in deleted_server_ids"
    );
    // No MoreAvailable in this fixture.
    assert!(
        !result.more_available,
        "MoreAvailable absent must remain false"
    );
}

/// Status-recovery parse lock: a Collection carrying `Status = "3"`
/// (invalid sync key, per MS-ASSYNC 2.2.3.23) must surface on
/// `SyncResult.status` so `EasSource::sync_folder`'s resync branch can act
/// on it. Task 4 covered the *behavioral* recovery; this test locks the
/// *parse-level* status plumbing that feeds it.
///
/// Without the parser surfacing Status, `result.status` would stay at the
/// `SyncResult::default()` value of `1` regardless of the wire value, and
/// the resync branch would never fire on a real status-3 response.
#[test]
fn parse_sync_response_surfaces_collection_status_3() {
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{stale}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "3"),
        ],
    );
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert_eq!(
        result.status, 3,
        "Collection/Status=3 must surface on SyncResult.status so sync_folder can resync"
    );
    assert_eq!(result.sync_key, "{stale}");
    // A status-3 response typically carries no Commands; assert the
    // vectors stay empty so the engine's resync path (which wipes the
    // cache and re-enters with sync_key "0") is not fed stale items.
    assert!(result.added.is_empty());
    assert!(result.updated.is_empty());
    assert!(result.deleted_server_ids.is_empty());
}

/// `parse_sync_response` must reject a tree whose root is not
/// Sync (page 0, token 0x05) with `WbxmlError::UnexpectedTag`. This locks
/// the `expect_tag` guard so a misrouted response (e.g. a FolderSync tree
/// handed to the Sync parser) fails loudly rather than returning a default
/// `SyncResult` that looks like success.
#[test]
fn parse_sync_response_rejects_non_sync_root() {
    let wrong_root = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    let err = parse_sync_response(&wrong_root).expect_err("must reject non-Sync root");
    assert!(
        matches!(err, WbxmlError::UnexpectedTag { .. }),
        "expected UnexpectedTag, got {err:?}"
    );
}

/// An empty Sync tree (root with no Collections child) must parse
/// successfully and yield a default `SyncResult` (status=1, empty vectors,
/// sync_key=""). This is the shape a server returns when it has nothing to
/// say; the engine must treat it as a no-op success, not an error.
#[test]
fn parse_sync_response_empty_tree_is_default_success() {
    let tree = WbxmlElement::container(PAGE_AIRSYNC, AS_SYNC, vec![]);
    let result = parse_sync_response(&tree).expect("parse");
    assert_eq!(result.status, 1, "default status is success");
    assert_eq!(result.sync_key, "");
    assert!(!result.more_available);
    assert!(result.added.is_empty());
    assert!(result.updated.is_empty());
    assert!(result.deleted_server_ids.is_empty());
}

// ---- R2 Task 4: live-probe findings (2026-08-02) ----
//
// The live probe against Exchange 2019 (16.1) showed `status 1, key
// <empty>, added 0` for the Sync bootstrap. Raw-dump evidence
// (examples/eas_sync_debug.rs): the server actually replied
// `Sync/Status=4` (protocol error) with NO Collections element —
// `x-ms-aserror: <Collection> node contains child node <Class> which
// appears out of order`. Two defects combined to hide that:
//   1. build_sync_request appended `<Class>` as the LAST child of `<Collection>` (after
//      `<Options>`). Per [MS-ASSYNC] the Class element is not a valid Collection child in protocol
//      14.0+ (CollectionId identifies the collection), so Exchange 16.1 rejects the whole request.
//   2. parse_sync_response ignored the top-level `<Status>`, so the rejection surfaced as a default
//      success with an empty sync key instead of an error the engine could act on.

/// A Sync response whose root carries `<Status>4</Status>` and NO
/// `Collections` element (Exchange's request-rejection shape — live
/// evidence: eas_sync_debug raw dump) must surface that status on
/// `SyncResult.status`, not read as default success with an empty key.
/// Collection-level Status (when present) remains authoritative.
#[test]
fn parse_sync_response_surfaces_top_level_status() {
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "4")],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert_eq!(
        result.status, 4,
        "top-level Sync/Status=4 (request rejected) must surface, not read as success"
    );
    assert!(result.sync_key.is_empty());
    assert!(result.added.is_empty());
}

/// When BOTH a top-level and a collection-level Status are present, the
/// collection-level value wins — it is the more specific signal.
#[test]
fn parse_sync_response_collection_status_overrides_top_level() {
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "2"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "3"),
        ],
    );
    let tree = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            WbxmlElement::container(PAGE_AIRSYNC, AS_COLLECTIONS, vec![collection]),
        ],
    );

    let result = parse_sync_response(&tree).expect("parse");

    assert_eq!(result.status, 3, "collection-level Status is authoritative");
    assert_eq!(result.sync_key, "2");
}
