// SPDX-License-Identifier: MPL-2.0
//! Sync-command tests, split along responsibilities (see the submodule docs).
//! Fixtures shared by more than one submodule live here and are reached via
//! `super::`.

use provider_eas::{
    calendar_write::{
        CalendarEventWrite, build_calendar_application_data, build_fixed_offset_tzi_base64,
    },
    commands::{tests_common::*, *},
};

#[path = "commands_sync/build_request_body.rs"]
mod build_request_body;
#[path = "commands_sync/build_request_collections.rs"]
mod build_request_collections;
#[path = "commands_sync/build_request_options.rs"]
mod build_request_options;
#[path = "commands_sync/calendar_write.rs"]
mod calendar_write;
#[path = "commands_sync/change_request.rs"]
mod change_request;
#[path = "commands_sync/change_response.rs"]
mod change_response;
#[path = "commands_sync/eas_item.rs"]
mod eas_item;
#[path = "commands_sync/item_estimate.rs"]
mod item_estimate;
#[path = "commands_sync/parse_email.rs"]
mod parse_email;
#[path = "commands_sync/parse_meeting.rs"]
mod parse_meeting;
#[path = "commands_sync/parse_sync_response.rs"]
mod parse_sync_response;

/// Convenience wrapper around the (currently stubbed) parser so the test
/// references the real function name. This mirrors the brief's
/// `parse_application_data(server_id, &elem) -> EasItem` signature.
fn parse_application_data_for_test(server_id: &str, elem: &WbxmlElement) -> EasItem {
    let mut item = EasItem {
        server_id: server_id.to_string(),
        ..Default::default()
    };
    parse_application_data(elem, &mut item);
    item
}

/// Build a single EAS email `ApplicationData` element carrying Subject +
/// From + To + Body[Type=2 HTML]. Shared by the Add and Change fixtures
/// below so the test body stays focused on the top-level orchestration.
fn fixture_email_app_data(subject: &str, from: &str, to: &str, body_html: &str) -> WbxmlElement {
    use provider_eas::wbxml::tags::{base, email, pages};
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(email::PAGE, email::SUBJECT, subject),
            WbxmlElement::text(email::PAGE, email::FROM, from),
            WbxmlElement::text(email::PAGE, email::TO, to),
            WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "2"),
                    WbxmlElement::text(pages::BASE, base::DATA, body_html),
                ],
            ),
        ],
    )
}

/// Build a Sync request, round-trip it through the WBXML codec, and
/// return the `Collection` element for positional assertions.
fn sync_collection_for(req: &SyncRequest, protocol_version: &str) -> WbxmlElement {
    let tree = build_sync_request(req, protocol_version);
    let back = round_trip(&tree);
    let collections = back
        .children
        .into_iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTIONS)
        .expect("missing Collections container");
    collections
        .children
        .into_iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_COLLECTION)
        .expect("missing Collection element")
}

/// `(page, token)` sequence of a Collection's children, for exact-order
/// assertions.
fn collection_child_tokens(collection: &WbxmlElement) -> Vec<(u8, u8)> {
    collection
        .children
        .iter()
        .map(|c| (c.page, c.token))
        .collect()
}
