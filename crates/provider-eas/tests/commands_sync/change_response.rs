// SPDX-License-Identifier: MPL-2.0
//! parse_sync_change_response: sync key/status, piggybacked commands, Add-ack Responses.

use super::*;

/// Parse a Sync Change response: Collections > Collection carries the new
/// SyncKey and the collection Status (MS-ASSYNC §2.2.3.23).
#[test]
fn sync_change_response_parses_sync_key_and_status() {
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "2"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, "5"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                ],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "2");
    assert_eq!(outcome.status, 1);
}

/// A non-1 collection status is surfaced (the client maps it to
/// CommandStatus); an absent Status defaults to 1 (success).
#[test]
fn sync_change_response_surfaces_non_success_status() {
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "0"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "3"),
                ],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "0");
    assert_eq!(outcome.status, 3);

    // Absent Status -> success default.
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "7")],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "7");
    assert_eq!(outcome.status, 1);
}

/// A non-Sync root is a parse error, not a silent success.
#[test]
fn sync_change_response_rejects_wrong_root() {
    let response = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    assert!(parse_sync_change_response(&response).is_err());
}

/// Phase B Task 9: a Sync response to a client-Commands (upsync) request
/// MAY itself carry server-side `Commands` in the response Collection
/// ([MS-ASSYNC] §2.2.2 — the server piggybacks pending changes onto the
/// upsync response). The parser must surface them via the same parse_item
/// path the downsync uses; discarding them risks silent divergence when
/// the caller adopts the rotated key.
#[test]
fn sync_change_response_parses_piggybacked_commands() {
    let add_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:42"),
            fixture_email_app_data("Piggy Subject", "p@x", "q@y", "<p>pg</p>"),
        ],
    );
    let change_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_CHANGE,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:7"),
            WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::text(
                    tags::email::PAGE,
                    tags::email::READ,
                    "0",
                )],
            ),
        ],
    );
    // EAS Delete is a CONTAINER carrying the ServerId as a child element
    // (MS-ASCMD 2.2.3.42.2) — the spec-conformant shape; the text-leaf
    // form is only accepted by the parser as a legacy-capture fallback.
    let delete_cmd = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_DELETE,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:13")],
    );
    let commands = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COMMANDS,
        vec![add_cmd, change_cmd, delete_cmd],
    );
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "9"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, "5"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    commands,
                ],
            )],
        )],
    );

    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "9");
    assert_eq!(outcome.status, 1);

    assert_eq!(outcome.piggybacked_added.len(), 1, "one piggybacked Add");
    let added = &outcome.piggybacked_added[0];
    assert_eq!(added.server_id, "5:42");
    assert_eq!(added.subject.as_deref(), Some("Piggy Subject"));
    assert_eq!(
        added.body_html.as_deref(),
        Some("<p>pg</p>"),
        "piggybacked Add must run the full parse_item / ApplicationData path"
    );

    assert_eq!(
        outcome.piggybacked_updated.len(),
        1,
        "one piggybacked Change"
    );
    assert_eq!(outcome.piggybacked_updated[0].server_id, "5:7");
    assert_eq!(outcome.piggybacked_updated[0].read, Some(false));

    assert_eq!(
        outcome.piggybacked_deleted,
        vec!["5:13".to_string()],
        "piggybacked Delete ServerId must be surfaced"
    );
}

/// A plain upsync response with NO server-side Commands parses with empty
/// piggybacked vectors — the common case must not change shape.
#[test]
fn sync_change_response_without_commands_has_empty_piggybacked() {
    let response = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "4"),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                ],
            )],
        )],
    );
    let outcome = parse_sync_change_response(&response).expect("parse");
    assert_eq!(outcome.new_key, "4");
    assert!(outcome.piggybacked_added.is_empty());
    assert!(outcome.piggybacked_updated.is_empty());
    assert!(outcome.piggybacked_deleted.is_empty());
}

// ============================================================================
// M8 calendar upsync Task 3 — parse_sync_change_response: Responses
// Add-ack + per-item Status parsing
// ([MS-ASCMD] §2.2.3.154 Responses; §2.2.3.7.2 Add (Sync); §2.2.3.24 Change;
// §2.2.3.42.2 Delete; §2.2.3.177.17 Status (Sync) — docs/Exchange/mscmd.txt)
// ============================================================================

/// Build a `Responses` Add item in the §4.5.3.2 example wire order
/// (ClientId, ServerId?, Status). `server_id: None` emits no ServerId
/// element — the shape of a FAILED add (the server only assigns the id on
/// success).
fn response_add_item(client_id: &str, status: &str, server_id: Option<&str>) -> WbxmlElement {
    let mut children = vec![WbxmlElement::text(PAGE_AIRSYNC, AS_CLIENT_ID, client_id)];
    if let Some(sid) = server_id {
        children.push(WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, sid));
    }
    children.push(WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status));
    WbxmlElement::container(PAGE_AIRSYNC, AS_ADD, children)
}

/// Build a `Responses` Change/Delete item ([MS-ASCMD] §2.2.3.24 /
/// §2.2.3.42.2): { ServerId, Status }. The command token is supplied by the
/// caller (`AS_CHANGE` / `AS_DELETE`).
fn response_status_item(command_token: u8, server_id: &str, status: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        command_token,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, server_id),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status),
        ],
    )
}

/// Wrap response Collection children in the full
/// `Sync > Collections > Collection` upsync-response envelope, so tests
/// route through the real parser entry instead of raw index chains.
fn upsync_response(collection_children: Vec<WbxmlElement>) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                collection_children,
            )],
        )],
    )
}

/// Fixture A: a successful calendar Add — Responses > Add { ClientId
/// "CalAdd-abc", ServerId "5:7", Status 1 } (§4.5.3.2 shape) plus the
/// rotated SyncKey and collection Status 1. Asserted directly AND after a
/// WBXML round trip.
#[test]
fn sync_change_response_parses_add_ack() {
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{rot-1}"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![response_add_item("CalAdd-abc", "1", Some("5:7"))],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");
    assert_eq!(outcome.new_key, "{rot-1}", "rotated key captured");
    assert_eq!(outcome.status, 1);
    assert_eq!(outcome.add_acks.len(), 1, "exactly one Add ack");
    let ack = &outcome.add_acks[0];
    assert_eq!(ack.client_id, "CalAdd-abc");
    assert_eq!(ack.status, 1);
    assert_eq!(ack.server_id.as_deref(), Some("5:7"));
    assert!(ack.success(), "status 1 ack must read as success");
    assert!(outcome.item_statuses.is_empty(), "no Change/Delete items");
    assert!(
        !outcome.has_piggybacked(),
        "no piggybacked Commands in this fixture"
    );

    // WBXML round trip: the ack must survive real encode/decode bytes, not
    // just the in-memory tree (locks the AS_RESPONSES 0x06 / AS_CLIENT_ID
    // 0x0C tokens through the codec's page handling).
    let back = round_trip(&tree);
    let outcome_rt = parse_sync_change_response(&back).expect("parse after round trip");
    assert_eq!(outcome_rt.add_acks, outcome.add_acks);
    assert_eq!(outcome_rt.new_key, "{rot-1}");
}

/// Fixture B: a FAILED Add (Status 6, no ServerId) plus per-item statuses
/// for a Change and a Delete. Status 6 per [MS-ASCMD] §2.2.3.177.17 is
/// "Error in client/server conversion" — the client sent a malformed or
/// invalid item; item-scoped, NOT transient ("stop sending the item").
#[test]
fn sync_change_response_parses_failed_add_and_change_delete_item_statuses() {
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{rot-2}"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![
                response_add_item("CalAdd-bad", "6", None),
                response_status_item(AS_CHANGE, "5:8", "1"),
                response_status_item(AS_DELETE, "5:9", "1"),
            ],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");

    // The failed Add: ack present, no ServerId, success() false.
    assert_eq!(outcome.add_acks.len(), 1);
    let ack = &outcome.add_acks[0];
    assert_eq!(ack.client_id, "CalAdd-bad");
    assert_eq!(ack.status, 6);
    assert_eq!(ack.server_id, None, "failed Add carries no ServerId");
    assert!(!ack.success());

    // Per-item statuses: Change and Delete both surface with their kinds.
    assert_eq!(outcome.item_statuses.len(), 2);
    let change = &outcome.item_statuses[0];
    assert_eq!(change.server_id, "5:8");
    assert_eq!(change.status, 1);
    assert_eq!(change.kind, ResponseItemKind::Change);
    assert!(change.success());
    let delete = &outcome.item_statuses[1];
    assert_eq!(delete.server_id, "5:9");
    assert_eq!(delete.status, 1);
    assert_eq!(delete.kind, ResponseItemKind::Delete);
    assert!(delete.success());
}

/// Fixture C (email-shaped regression): a response with NO Responses element
/// — the common email upsync shape — must parse with both new vectors empty
/// and everything else unchanged.
#[test]
fn sync_change_response_without_responses_has_empty_ack_vectors() {
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "4"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");
    assert_eq!(outcome.new_key, "4");
    assert_eq!(outcome.status, 1);
    assert!(outcome.add_acks.is_empty());
    assert!(outcome.item_statuses.is_empty());
    assert!(outcome.piggybacked_added.is_empty());
    assert!(outcome.piggybacked_updated.is_empty());
    assert!(outcome.piggybacked_deleted.is_empty());
}

/// Fixture D: the [MS-ASSYNC] §2.2.2 piggyback case AND Responses in ONE
/// response — a Commands block (server-side changes) alongside a Responses
/// block (acks for the client's commands). Both must parse; neither may
/// starve the other.
#[test]
fn sync_change_response_parses_piggybacked_commands_and_responses_together() {
    let piggybacked_add = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:42"),
            fixture_email_app_data("Piggy + Acks", "p@x", "q@y", "<p>both</p>"),
        ],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![piggybacked_add]);
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "9"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        commands,
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![
                response_add_item("CalAdd-both", "1", Some("5:77")),
                response_status_item(AS_CHANGE, "5:78", "8"),
            ],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");

    // Piggybacked Commands still parse via the email path.
    assert_eq!(outcome.new_key, "9");
    assert!(outcome.has_piggybacked());
    assert_eq!(outcome.piggybacked_added.len(), 1);
    assert_eq!(outcome.piggybacked_added[0].server_id, "5:42");
    assert_eq!(
        outcome.piggybacked_added[0].subject.as_deref(),
        Some("Piggy + Acks")
    );

    // Responses parse alongside them.
    assert_eq!(outcome.add_acks.len(), 1);
    assert_eq!(outcome.add_acks[0].server_id.as_deref(), Some("5:77"));
    assert_eq!(outcome.item_statuses.len(), 1);
    // Status 8 = "Object not found" ([MS-ASCMD] §2.2.3.177.17) — the
    // ServerId is no longer valid on the server; NOT a success.
    assert_eq!(outcome.item_statuses[0].status, 8);
    assert!(!outcome.item_statuses[0].success());
}

/// Malformed-shape policy (permissive, like the rest of the file): an Add
/// without a ClientId warns and is skipped; a Change/Delete without a
/// ServerId warns and is skipped; unknown Response kinds (`Fetch`,
/// [MS-ASCMD] §2.2.3.67.2) are debug-skipped. The well-formed siblings in
/// the same Responses block still parse.
#[test]
fn sync_change_response_skips_malformed_and_unknown_response_items() {
    let fetch = WbxmlElement::container(
        PAGE_AIRSYNC,
        tags::airsync::FETCH,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "1:14"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        ],
    );
    let tree = upsync_response(vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "5"),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_RESPONSES,
            vec![
                // Add with NO ClientId — uncorrelatable, skipped.
                WbxmlElement::container(
                    PAGE_AIRSYNC,
                    AS_ADD,
                    vec![
                        WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "5:1"),
                        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    ],
                ),
                // Unknown kind: a Fetch response (§4.5.2.2 shape).
                fetch,
                // Change with NO ServerId — skipped.
                WbxmlElement::container(
                    PAGE_AIRSYNC,
                    AS_CHANGE,
                    vec![WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1")],
                ),
                // The well-formed siblings.
                response_add_item("CalAdd-ok", "1", Some("5:2")),
                response_status_item(AS_DELETE, "5:3", "1"),
            ],
        ),
    ]);

    let outcome = parse_sync_change_response(&tree).expect("parse");
    assert_eq!(
        outcome.add_acks,
        vec![CalendarAddAck {
            client_id: "CalAdd-ok".to_string(),
            status: 1,
            server_id: Some("5:2".to_string()),
        }],
        "only the Add carrying a ClientId is acked"
    );
    assert_eq!(
        outcome.item_statuses,
        vec![CalendarItemStatus {
            server_id: "5:3".to_string(),
            status: 1,
            kind: ResponseItemKind::Delete,
        }],
        "only the Delete carrying a ServerId surfaces"
    );
}
