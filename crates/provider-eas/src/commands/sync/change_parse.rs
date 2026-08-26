// SPDX-License-Identifier: MPL-2.0
// Sync Change response parsing (client acks).

use super::{
    change::{CalendarAddAck, CalendarItemStatus, ResponseItemKind, SyncChangeOutcome},
    parse_item::parse_sync_commands,
};
use crate::commands::{
    AS_ADD, AS_CHANGE, AS_CLIENT_ID, AS_COLLECTION, AS_COLLECTIONS, AS_COMMANDS, AS_DELETE,
    AS_RESPONSES, AS_SERVER_ID, AS_STATUS, AS_SYNC, AS_SYNC_KEY, PAGE_AIRSYNC, WbxmlElement,
    WbxmlError, expect_tag, text_value,
};
/// Parse a Sync response to a client-side Change request. Returns a
/// [`SyncChangeOutcome`]: the server echoes the rotated SyncKey and a
/// per-collection Status (MS-ASSYNC §2.2.3.23), plus — under the response
/// Collection's `Responses` element ([MS-ASCMD] §2.2.3.154) — a per-item
/// Add acknowledgement for each client Add ([`CalendarAddAck`]: ClientId →
/// the assigned ServerId, §2.2.3.7.2) and per-item statuses for client
/// Change/Delete commands ([`CalendarItemStatus`], §2.2.3.24 / §2.2.3.42.2).
/// Per §2.2.3.154 the server only responds for successful additions and
/// FAILED changes/deletions — commands with no entry under `Responses`
/// succeeded. An absent Status defaults to 1 (success); an absent SyncKey
/// yields an empty string (caller decides whether to persist it).
///
/// The response Collection MAY also carry server-side `Commands` ([MS-ASSYNC]
/// §2.2.2 — changes the server had pending are piggybacked onto the upsync
/// response). Those are parsed via the same Commands/`parse_item` path the
/// downsync uses and surfaced on the outcome's `piggybacked_*` vectors —
/// discarding them while adopting the rotated key would silently diverge
/// from the server.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_sync_change_response(root: &WbxmlElement) -> Result<SyncChangeOutcome, WbxmlError> {
    expect_tag(root, PAGE_AIRSYNC, AS_SYNC)?;

    let mut outcome = SyncChangeOutcome {
        status: 1, // success default per MS-ASSYNC 2.2.3.23
        ..SyncChangeOutcome::default()
    };

    // Top-level Status (request-level rejection, e.g. 4 = invalid request)
    // applies first; a collection-level Status (the more specific signal)
    // overrides it — same rule as `parse_sync_response`.
    for child in &root.children {
        if child.page == PAGE_AIRSYNC
            && child.token == AS_STATUS
            && let Ok(s) = text_value(child)
            && let Ok(n) = s.parse::<u32>()
        {
            outcome.status = n;
        }
    }
    for child in &root.children {
        if (child.page, child.token) != (PAGE_AIRSYNC, AS_COLLECTIONS) {
            continue;
        }
        for col_el in &child.children {
            if (col_el.page, col_el.token) != (PAGE_AIRSYNC, AS_COLLECTION) {
                continue;
            }
            for c in &col_el.children {
                match (c.page, c.token) {
                    (PAGE_AIRSYNC, AS_SYNC_KEY) => outcome.new_key = text_value(c)?,
                    (PAGE_AIRSYNC, AS_STATUS) => {
                        if let Ok(s) = text_value(c)
                            && let Ok(n) = s.parse::<u32>()
                        {
                            outcome.status = n;
                        }
                    }
                    (PAGE_AIRSYNC, AS_COMMANDS) => {
                        parse_sync_commands(
                            c,
                            &mut outcome.piggybacked_added,
                            &mut outcome.piggybacked_updated,
                            &mut outcome.piggybacked_deleted,
                        )?;
                    }
                    (PAGE_AIRSYNC, AS_RESPONSES) => {
                        parse_sync_responses(c, &mut outcome.add_acks, &mut outcome.item_statuses);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(outcome)
}

/// Walk a `Responses` element ([MS-ASCMD] §2.2.3.154) — the server's
/// per-item echoes of the client's upsync commands. Wire shapes (all
/// AirSync page-0 tokens, [MS-ASWBXML] §2.1.2.1.1):
/// - `Add` (0x07, §2.2.3.7.2): `{ ClientId 0x0C, ServerId? 0x0D, Status 0x0E }` — the §4.5.3.2
///   example order; the ServerId is assigned on success only.
/// - `Change` (0x08, §2.2.3.24) / `Delete` (0x09, §2.2.3.42.2): `{ ServerId, Status }`.
///
/// Malformed-shape policy mirrors the file's permissive parse
/// (text_value + warn/default): an Add without a ClientId warns and is
/// skipped (the ack cannot be correlated); a Change/Delete without a
/// ServerId warns and is skipped; unknown Response kinds (`Fetch` 0x0A,
/// §2.2.3.67.2) are debug-skipped; a non-numeric Status keeps the success
/// default of 1 with a warn. Nothing is invented — only what the server
/// sent is surfaced.
fn parse_sync_responses(
    responses_el: &WbxmlElement,
    add_acks: &mut Vec<CalendarAddAck>,
    item_statuses: &mut Vec<CalendarItemStatus>,
) {
    for item in &responses_el.children {
        match (item.page, item.token) {
            (PAGE_AIRSYNC, AS_ADD) => {
                let mut client_id = String::new();
                let mut server_id: Option<String> = None;
                let mut status = 1u32; // success default per §2.2.3.177.17
                for child in &item.children {
                    match (child.page, child.token) {
                        (PAGE_AIRSYNC, AS_CLIENT_ID) => {
                            client_id = text_value(child).unwrap_or_default();
                        }
                        (PAGE_AIRSYNC, AS_SERVER_ID) => {
                            server_id = Some(text_value(child).unwrap_or_default());
                        }
                        (PAGE_AIRSYNC, AS_STATUS) => {
                            if let Ok(s) = text_value(child) {
                                match s.parse::<u32>() {
                                    Ok(n) => status = n,
                                    Err(_) => log::warn!(
                                        "Sync Responses: malformed Add Status \"{s}\"; \
                                         keeping the default of 1"
                                    ),
                                }
                            }
                        }
                        // Class / ApplicationData (§2.2.3.7.2 lists both as
                        // optional Responses-Add children; the 16.0/16.1
                        // ApplicationData echo) are not modeled — ignored.
                        _ => {}
                    }
                }
                if client_id.is_empty() {
                    log::warn!(
                        "Sync Responses: Add without ClientId — the ack cannot be correlated, skipping"
                    );
                    continue;
                }
                add_acks.push(CalendarAddAck {
                    client_id,
                    status,
                    // §2.2.3.7.2: the server assigns the ServerId on
                    // success; a failed add has none to correlate, even if
                    // a stray element arrived.
                    server_id: if status == 1 {
                        server_id.filter(|s| !s.is_empty())
                    } else {
                        None
                    },
                });
            }
            (PAGE_AIRSYNC, AS_CHANGE | AS_DELETE) => {
                let kind = if item.token == AS_CHANGE {
                    ResponseItemKind::Change
                } else {
                    ResponseItemKind::Delete
                };
                let mut server_id = String::new();
                let mut status = 1u32; // success default per §2.2.3.177.17
                for child in &item.children {
                    match (child.page, child.token) {
                        (PAGE_AIRSYNC, AS_SERVER_ID) => {
                            server_id = text_value(child).unwrap_or_default();
                        }
                        (PAGE_AIRSYNC, AS_STATUS) => {
                            if let Ok(s) = text_value(child) {
                                match s.parse::<u32>() {
                                    Ok(n) => status = n,
                                    Err(_) => log::warn!(
                                        "Sync Responses: malformed {kind:?} Status \"{s}\"; \
                                         keeping the default of 1"
                                    ),
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if server_id.is_empty() {
                    log::warn!(
                        "Sync Responses: {kind:?} response without ServerId — the status cannot be \
                         correlated, skipping"
                    );
                    continue;
                }
                item_statuses.push(CalendarItemStatus {
                    server_id,
                    status,
                    kind,
                });
            }
            // Unknown Response kinds — e.g. `Fetch` (§2.2.3.67.2, the
            // §4.5.2.2 example) — have no consumer yet; skip at debug.
            (page, token) => {
                log::debug!(
                    "Sync Responses: skipping unhandled response item ({page:#04x}, {token:#04x})"
                );
            }
        }
    }
}
