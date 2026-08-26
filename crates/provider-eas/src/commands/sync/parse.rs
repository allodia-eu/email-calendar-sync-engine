// SPDX-License-Identifier: MPL-2.0
// Sync response envelope parsing (collection dispatch by class).

use super::parse_item::{
    parse_calendar_sync_commands, parse_contacts_sync_commands, parse_sync_commands,
};
use crate::commands::{
    AS_COLLECTION, AS_COLLECTIONS, AS_COMMANDS, AS_MORE_AVAILABLE, AS_STATUS, AS_SYNC, AS_SYNC_KEY,
    PAGE_AIRSYNC, SyncResult, WbxmlElement, WbxmlError, expect_tag, text_value,
};
/// Parse a Sync response.
///
/// The class-unaware default entry: behaves exactly like
/// [`parse_sync_response_for_class`] called with an empty class — i.e. the
/// Email-shaped `ApplicationData` path (`added` / `updated`), calendar
/// vectors empty. Existing callers and tests keep this signature untouched.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_sync_response(root: &WbxmlElement) -> Result<SyncResult, WbxmlError> {
    parse_sync_response_for_class(root, "")
}

/// Parse a Sync response whose collection was requested with `class`
/// (M8 Task 4 seam).
///
/// Contract (locked by the seam tests below):
/// - `"Calendar"` → Add/Change `ApplicationData` is routed to the MS-ASCAL parser
///   (`calendar::parse_calendar_application_data`, Tasks 2-3); the typed items surface on
///   `SyncResult::calendar_added` / `calendar_updated` WITH their ServerIds, and `added` /
///   `updated` stay empty.
/// - `"Contacts"` → Add/Change `ApplicationData` is routed to the MS-ASCNTC parser
///   (`contacts::parse_contacts_application_data`, M8-C task 1); the typed items surface on
///   `SyncResult::contacts_added` / `contacts_updated` WITH their ServerIds, and `added` /
///   `updated` plus the calendar vectors stay empty.
/// - `"Email"` and `""` (the pre-M8 default) → today's Email-shaped parse, bit-for-bit;
///   calendar/contacts vectors stay empty.
/// - Any other class (`"Tasks"`, `"Notes"`) falls through to the Email-shaped parser — there is no
///   typed parser for them yet; the fallthrough is logged at `debug`, never silently invented.
/// - Deletes are class-agnostic on the wire ([MS-ASSYNC] §2.2.2.4) and always land in
///   `deleted_server_ids`.
/// - `sync_key` / `more_available` / `status` parse identically for every class.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_sync_response_for_class(
    root: &WbxmlElement,
    class: &str,
) -> Result<SyncResult, WbxmlError> {
    expect_tag(root, PAGE_AIRSYNC, AS_SYNC)?;

    let mut result = SyncResult::default();
    // Top-level Status (request-level rejection, e.g. 4 = invalid request)
    // precedes Collections on the wire per [MS-ASSYNC]; apply it first so a
    // collection-level Status (the more specific signal) overrides it below.
    for child in &root.children {
        if child.page == PAGE_AIRSYNC
            && child.token == AS_STATUS
            && let Ok(s) = text_value(child)
            && let Ok(n) = s.parse::<u32>()
        {
            result.status = n;
        }
    }
    for child in &root.children {
        if let (PAGE_AIRSYNC, AS_COLLECTIONS) = (child.page, child.token) {
            for col_el in &child.children {
                if col_el.page == PAGE_AIRSYNC && col_el.token == AS_COLLECTION {
                    parse_sync_collection(col_el, &mut result, class)?;
                }
            }
        }
    }
    Ok(result)
}

fn parse_sync_collection(
    col: &WbxmlElement,
    result: &mut SyncResult,
    class: &str,
) -> Result<(), WbxmlError> {
    for child in &col.children {
        match (child.page, child.token) {
            (PAGE_AIRSYNC, AS_SYNC_KEY) => result.sync_key = text_value(child)?,
            (PAGE_AIRSYNC, AS_MORE_AVAILABLE) => result.more_available = true,
            (PAGE_AIRSYNC, AS_STATUS) => {
                // MS-ASSYNC 2.2.3.23 collection status. Surface the parsed
                // value on `SyncResult.status` so callers (notably
                // `EasSource::sync_folder`'s status-3 resync branch) can act
                // on it. The wire value is a decimal string; a non-numeric or
                // missing value leaves the default success status in place
                // rather than aborting the whole parse.
                if let Ok(s) = text_value(child)
                    && let Ok(n) = s.parse::<u32>()
                {
                    result.status = n;
                }
            }
            (PAGE_AIRSYNC, AS_COMMANDS) => {
                parse_sync_commands_for_class(child, result, class)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Route a downsync `Commands` element by the request's collection class
/// (M8 Task 4 / M8-C task 1 seam): `"Calendar"` fills `calendar_added` /
/// `calendar_updated` via the MS-ASCAL parser, `"Contacts"` fills
/// `contacts_added` / `contacts_updated` via the MS-ASCNTC parser;
/// everything else keeps the Email-shaped `EasItem` path bit-for-bit.
/// Deletes are class-agnostic and share `deleted_server_ids` on every route.
fn parse_sync_commands_for_class(
    commands_el: &WbxmlElement,
    result: &mut SyncResult,
    class: &str,
) -> Result<(), WbxmlError> {
    match class {
        "Calendar" => parse_calendar_sync_commands(
            commands_el,
            &mut result.calendar_added,
            &mut result.calendar_updated,
            &mut result.deleted_server_ids,
        ),
        "Contacts" => parse_contacts_sync_commands(
            commands_el,
            &mut result.contacts_added,
            &mut result.contacts_updated,
            &mut result.deleted_server_ids,
        ),
        // The pre-M8 default (`""`) and explicit Email stay on the
        // Email-shaped path — the golden wire-shape regression line.
        // Tasks/Notes fall through to the Email-shaped parser today (no
        // typed parser exists yet); the fallthrough is visible in logs,
        // never silent.
        other => {
            if !matches!(other, "Email" | "") {
                log::debug!(
                    "Sync parse: no typed ApplicationData parser for class \"{other}\" yet; \
                     falling through to the Email-shaped path"
                );
            }
            parse_sync_commands(
                commands_el,
                &mut result.added,
                &mut result.updated,
                &mut result.deleted_server_ids,
            )
        }
    }
}
