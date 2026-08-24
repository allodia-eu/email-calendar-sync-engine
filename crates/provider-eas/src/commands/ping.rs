// SPDX-License-Identifier: MPL-2.0
use super::*;

// ============================================================================
// Ping
// ============================================================================

/// Build a Ping request.
pub fn build_ping_request(req: &PingRequest) -> WbxmlElement {
    let folder_elements: Vec<WbxmlElement> = req
        .monitored_collections
        .iter()
        .map(|c| {
            WbxmlElement::container(
                PAGE_PING,
                PING_FOLDER,
                vec![
                    WbxmlElement::text(PAGE_PING, PING_ID, c.collection_id.clone()),
                    WbxmlElement::text(PAGE_PING, PING_CLASS, c.class.clone()),
                ],
            )
        })
        .collect();

    let children = vec![
        WbxmlElement::text(
            PAGE_PING,
            PING_HEARTBEAT_INTERVAL,
            req.heartbeat_interval.to_string(),
        ),
        WbxmlElement::container(PAGE_PING, PING_FOLDERS, folder_elements),
    ];

    WbxmlElement::container(PAGE_PING, PING_PING, children)
}

pub fn parse_ping_response(root: &WbxmlElement) -> Result<PingResult, WbxmlError> {
    let mut result = PingResult::default();
    for child in &root.children {
        if child.page == PAGE_PING && child.token == PING_STATUS {
            let status = text_value(child).unwrap_or_default();
            // MS-ASCMD 2.2.3.177.11 + mailkit_arkts PingStatus:
            //   1 = "heartbeat interval expired before any changes occurred"
            //   2 = "changes occurred in at least one monitored folder"
            // (historically inverted here — a status-1 expiry is NOT a change
            // signal; a status-2 answer with Folders is).
            result.status = match status.as_str() {
                "1" => "Expired".to_string(),
                "2" => "Changes".to_string(),
                other => other.to_string(),
            };
        } else if child.page == PAGE_PING && child.token == PING_HEARTBEAT_INTERVAL {
            result.heartbeat_interval = text_value(child).ok().and_then(|s| s.parse().ok());
        } else if child.page == PAGE_PING && child.token == PING_FOLDERS {
            // Changed-collection ServerIds (the Folder elements' text). Per
            // MS-ASCMD 2.2.3.75.2 the Folders element only appears when
            // changes occurred — collect them regardless of the Status value
            // (defense for servers that mislabel).
            for folder in &child.children {
                if folder.page == PAGE_PING && folder.token == PING_FOLDER {
                    if let Ok(id) = text_value(folder) {
                        if !id.is_empty() {
                            result.folders.push(id);
                        }
                    }
                }
            }
        }
    }
    if result.status.is_empty() {
        // Absent Status (a bare <Ping/> response, e.g. the empty-body
        // no-changes case): treat as a clean expiry, NOT as changes — the
        // alternative fires spurious sync rounds every heartbeat.
        result.status = "Expired".to_string();
    }
    Ok(result)
}
