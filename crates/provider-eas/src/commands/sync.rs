// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};

use super::*;
use crate::{
    calendar::{CalendarEventProps, parse_location_16x},
    calendar_write::{CalendarEventWrite, build_calendar_application_data},
    contacts::ContactsContactProps,
    wbxml::tags::{base, pages},
};

// ============================================================================
// Sync
// ============================================================================

/// Build a Sync request for a single collection.
///
/// `Collection` children follow the strict order of [MS-ASCMD] §2.2.3.29.2:
/// SyncKey, CollectionId, (Supported), DeletesAsMoves, GetChanges,
/// WindowSize, (ConversationMode), Options, Commands — this builder emits
/// `Supported` when [`SyncRequest::supported`] is `Some(non-empty)` and
/// never emits ConversationMode/Commands.
///
/// `<DeletesAsMoves/>` is emitted unconditionally right after CollectionId
/// (or after `Supported` when that is present):
/// every [MS-ASCMD] §4.5 Sync example sends it, and the empty form means
/// TRUE (§2.2.3.43) — deletes move to the Deleted Items folder instead of
/// being hard-deleted, matching client behavior.
///
/// `protocol_version` gates the `GetChanges` element: per [MS-ASSYNC]
/// §2.2.2.9 it is not valid in 16.1 (the server sends changes by default and
/// rejects requests carrying it — live evidence: eas_sync_bisect against
/// Exchange 2019, every GetChanges variant answered top-level Status=4).
/// Pre-16.1 it is required, so it is emitted for any other version string.
///
/// `Options` is emitted when a `FilterType` (`filter_age_days != 0`), a
/// `BodyPreference` (`fetch_body`), or a MIME option (`mime_support` /
/// `mime_truncation` `Some`) is requested, with FilterType as the FIRST
/// child ([MS-ASCMD] §2.2.3.125.6). `MIMESupport` / `MIMETruncation` follow
/// the BodyPreference — the §2.2.3.125.6 Options child order: FilterType?,
/// Class?, ConversationMode?, MaxItems?, BodyPreference*, MIMESupport?,
/// MIMETruncation?, RightsManagementSupport?.
pub fn build_sync_request(req: &SyncRequest, protocol_version: &str) -> WbxmlElement {
    let mut collection_children = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, req.sync_key.clone()),
        WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, req.collection_id.clone()),
    ];

    // [MS-ASCMD] §2.2.3.179 / §2.2.3.29.2: `Supported` names the schema
    // elements the client supports for this collection class (ghosting
    // control for future editing) and sits between CollectionId and
    // DeletesAsMoves. Each entry is emitted as an empty tag — the §4.24
    // shape. Only Some(non-empty) emits: None/Some([]) keep the request
    // byte-identical to the pre-Supported shape (per rule 1 an absent
    // Supported ghosts nothing; the wire-level `<Supported/>` "ghost
    // everything" form of rule 3 is deliberately unreachable).
    if let Some(supported) = req.supported.as_deref().filter(|s| !s.is_empty()) {
        collection_children.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_SUPPORTED,
            supported
                .iter()
                .map(|e| WbxmlElement::empty(e.page, e.token))
                .collect(),
        ));
    }

    // [MS-ASCMD] §2.2.3.29.2 order: DeletesAsMoves directly after
    // CollectionId (or Supported, when emitted above). Empty element =
    // TRUE (§2.2.3.43): server-side deletes go to Deleted Items, not away.
    collection_children.push(WbxmlElement::empty(PAGE_AIRSYNC, AS_DELETES_AS_MOVES));

    if protocol_version != "16.1" {
        collection_children.push(WbxmlElement::empty(PAGE_AIRSYNC, AS_GET_CHANGES));
    }

    if req.window_size != 0 {
        collection_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_WINDOW_SIZE,
            req.window_size.to_string(),
        ));
    }

    // Per [MS-ASSYNC] 2.2.3.25 / [MS-ASCMD] §2.2.3.125.6 — `Options` inside a
    // `Collection` controls how the server synchronizes it. Child order per
    // the Options (Sync) schema: FilterType FIRST, then BodyPreference.
    let mut options_children: Vec<WbxmlElement> = Vec::new();

    // FilterType (page 0, 0x18) bounds the sync to a time window
    // ([MS-ASCMD] §2.2.3.68.2; 0 = no filter, hence omitted then). Mirrors
    // `build_get_item_estimate_request`, which already sends it. Emitted
    // ahead of BodyPreference and even when `fetch_body` is false, so
    // header-only rounds honor the same age window (sticky-options note in
    // §2.2.3.125.6 makes the explicit block important: an omitted Options
    // reuses the PREVIOUS block).
    if req.filter_age_days != 0 {
        options_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_FILTER_TYPE,
            req.filter_age_days.to_string(),
        ));
    }

    // AirSyncBase `BodyPreference` with `Type=2` (HTML) so the server returns
    // message bodies. Gated on `fetch_body` so header-only sync rounds stay
    // cheap. Code-page ids: AirSyncBase = 17 (pages::BASE); tokens are
    // `BodyPreference` (0x05) and `Type` (0x06) per tags::base.
    //
    // When `truncation_size` is set, a `TruncationSize` child (token 0x07 —
    // verified against MS-ASWBXML.txt §2.1.2.1.18, AirSyncBase page 17 table)
    // follows `Type`, capping the per-item body payload the server returns
    // (children order per MS-ASAIRS BodyPreference: Type, TruncationSize,
    // AllOrNone). Larger bodies come back `Truncated=1` and are completed
    // on demand via ItemOperations (`fetch_body`).
    if req.fetch_body {
        let mut pref_children = vec![WbxmlElement::text(pages::BASE, tags::base::TYPE, "2")];
        if let Some(truncation_size) = req.truncation_size {
            pref_children.push(WbxmlElement::text(
                pages::BASE,
                tags::base::TRUNCATION_SIZE,
                truncation_size.to_string(),
            ));
        }
        // MS-ASAIRS 2.2.2.35.4: the server only returns `Body > Preview`
        // (the message-list snippet) when the BodyPreference carries a
        // Preview child (0-255 = max chars). Schema order keeps it LAST
        // (Type, TruncationSize, AllOrNone, Preview). Without it every
        // synced message had an empty snippet (live finding 2026-08-04).
        pref_children.push(WbxmlElement::text(pages::BASE, tags::base::PREVIEW, "255"));
        options_children.push(WbxmlElement::container(
            pages::BASE,
            tags::base::BODY_PREFERENCE,
            pref_children,
        ));
    }

    // MIMESupport / MIMETruncation ([MS-ASCMD] §2.2.3.110.3 / §2.2.3.111)
    // follow BodyPreference per the §2.2.3.125.6 Options child order. Both
    // are page-0 AirSync tokens (0x22 / 0x23). Emitted only when Some —
    // None keeps the request byte-for-byte identical to the pre-MIME shape
    // (an absent MIMESupport reads as 0 = never send MIME on the server).
    if let Some(level) = req.mime_support {
        options_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_MIME_SUPPORT,
            level.to_string(),
        ));
    }
    if let Some(level) = req.mime_truncation {
        options_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_MIME_TRUNCATION,
            level.to_string(),
        ));
    }

    if !options_children.is_empty() {
        collection_children.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_OPTIONS,
            options_children,
        ));
    }

    // NOTE: no `airsync:Class` element is emitted. Per [MS-ASSYNC] §2.2.2.11
    // Class is only a valid `Collection` child in protocol 2.5/12.x; in 14.0+
    // `CollectionId` identifies the collection, and Exchange 16.1 rejects a
    // request carrying Class with top-level Status=4 ("<Class> ... appears
    // out of order" — live evidence: eas_sync_debug raw dump, 2026-08-02).
    let collection = WbxmlElement::container(PAGE_AIRSYNC, AS_COLLECTION, collection_children);

    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}

/// Deprecated helper retained for callers that still import it. Returns the
/// single-collection token — `Collections` is now its own constant.
#[allow(non_snake_case)]
#[deprecated(note = "use AS_COLLECTIONS constant directly")]
fn AS_COLLECTIONS_CONTAINER() -> u8 {
    AS_COLLECTIONS
}

/// Parse a Sync response.
///
/// The class-unaware default entry: behaves exactly like
/// [`parse_sync_response_for_class`] called with an empty class — i.e. the
/// Email-shaped `ApplicationData` path (`added` / `updated`), calendar
/// vectors empty. Existing callers and tests keep this signature untouched.
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

/// Shared Add/Change/Delete walk over a `Commands` element. The envelope
/// classification (Add vs Change vs Delete) and the Delete ServerId
/// extraction (class-agnostic on the wire, [MS-ASSYNC] §2.2.2.4) live here
/// ONCE; the Add/Change payload parse is supplied by the caller's closure —
/// `parse_item` for the Email-shaped path, `parse_calendar_item` for the
/// Calendar class, `parse_contacts_item` for the Contacts class.
fn walk_sync_commands(
    commands_el: &WbxmlElement,
    deleted: &mut Vec<String>,
    mut on_add_change: impl FnMut(bool /* is_add */, &WbxmlElement) -> Result<(), WbxmlError>,
) -> Result<(), WbxmlError> {
    for cmd in &commands_el.children {
        match (cmd.page, cmd.token) {
            (PAGE_AIRSYNC, AS_ADD) | (PAGE_AIRSYNC, AS_CHANGE) => {
                on_add_change(cmd.token == AS_ADD, cmd)?;
            }
            (PAGE_AIRSYNC, AS_DELETE) => {
                // MS-ASCMD 2.2.3.42.2: <Delete> is a CONTAINER whose ServerId
                // is a CHILD element — the same shape Add/Change use. Reading
                // the Delete element's OWN text instead yielded empty server
                // ids, which hashed to a phantom uid and deleted zero rows —
                // the root cause of "OWA deletes never sync" (wire-verified
                // 2026-08-04: every Delete parsed as ""). The element-text
                // fallback keeps older captures (which put the id in the
                // text) parseable. Empty ids are dropped, never pushed.
                let id = cmd
                    .children
                    .iter()
                    .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_SERVER_ID)
                    .map(text_value)
                    .transpose()?
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| text_value(cmd).unwrap_or_default());
                if !id.is_empty() {
                    deleted.push(id);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Email-shaped `Commands` parse: Add and Change both go through `parse_item`
/// into `EasItem` vectors. Shared by `parse_sync_collection` (downsync, via
/// `parse_sync_commands_for_class`) and `parse_sync_change_response`
/// (server-piggybacked Commands on an upsync response, [MS-ASSYNC] §2.2.2) so
/// both directions parse identically.
fn parse_sync_commands(
    commands_el: &WbxmlElement,
    added: &mut Vec<EasItem>,
    updated: &mut Vec<EasItem>,
    deleted: &mut Vec<String>,
) -> Result<(), WbxmlError> {
    walk_sync_commands(commands_el, deleted, |is_add, cmd| {
        let item = parse_item(cmd)?;
        if is_add {
            added.push(item);
        } else {
            updated.push(item);
        }
        Ok(())
    })
}

/// Calendar-class `Commands` parse (M8 Task 4 seam): Add/Change payloads are
/// routed to the MS-ASCAL ApplicationData parser and surface as
/// [`CalendarItemWithId`] (ServerId + typed props); deletes share the same
/// class-agnostic `deleted` vector as the Email path.
fn parse_calendar_sync_commands(
    commands_el: &WbxmlElement,
    added: &mut Vec<CalendarItemWithId>,
    updated: &mut Vec<CalendarItemWithId>,
    deleted: &mut Vec<String>,
) -> Result<(), WbxmlError> {
    walk_sync_commands(commands_el, deleted, |is_add, cmd| {
        let item = parse_calendar_item(cmd)?;
        if is_add {
            added.push(item);
        } else {
            updated.push(item);
        }
        Ok(())
    })
}

/// Parse one Calendar-class Add/Change command element: the `ServerId`
/// envelope child plus the `ApplicationData` routed to
/// [`crate::calendar::parse_calendar_application_data`] (Tasks 2-3). Mirrors
/// `parse_item`'s envelope walk for the Email path.
fn parse_calendar_item(cmd_el: &WbxmlElement) -> Result<CalendarItemWithId, WbxmlError> {
    let mut server_id = String::new();
    let mut props = CalendarEventProps::default();
    for child in &cmd_el.children {
        match (child.page, child.token) {
            (PAGE_AIRSYNC, AS_SERVER_ID) => server_id = text_value(child)?,
            (PAGE_AIRSYNC, AS_APPLICATION_DATA) => {
                props = crate::calendar::parse_calendar_application_data(child)?;
            }
            _ => {}
        }
    }
    Ok(CalendarItemWithId { server_id, props })
}

/// Contacts-class `Commands` parse (M8-C task 1 seam): Add/Change payloads
/// are routed to the MS-ASCNTC ApplicationData parser and surface as
/// [`ContactsItemWithId`] (ServerId + typed props); deletes share the same
/// class-agnostic `deleted` vector as every other path. Mirrors
/// `parse_calendar_sync_commands`.
fn parse_contacts_sync_commands(
    commands_el: &WbxmlElement,
    added: &mut Vec<ContactsItemWithId>,
    updated: &mut Vec<ContactsItemWithId>,
    deleted: &mut Vec<String>,
) -> Result<(), WbxmlError> {
    walk_sync_commands(commands_el, deleted, |is_add, cmd| {
        let item = parse_contacts_item(cmd)?;
        if is_add {
            added.push(item);
        } else {
            updated.push(item);
        }
        Ok(())
    })
}

/// Parse one Contacts-class Add/Change command element: the `ServerId`
/// envelope child plus the `ApplicationData` routed to
/// [`crate::contacts::parse_contacts_application_data`]. Mirrors
/// `parse_calendar_item`'s envelope walk.
fn parse_contacts_item(cmd_el: &WbxmlElement) -> Result<ContactsItemWithId, WbxmlError> {
    let mut server_id = String::new();
    let mut props = ContactsContactProps::default();
    for child in &cmd_el.children {
        match (child.page, child.token) {
            (PAGE_AIRSYNC, AS_SERVER_ID) => server_id = text_value(child)?,
            (PAGE_AIRSYNC, AS_APPLICATION_DATA) => {
                props = crate::contacts::parse_contacts_application_data(child)?;
            }
            _ => {}
        }
    }
    Ok(ContactsItemWithId { server_id, props })
}

fn parse_item(item_el: &WbxmlElement) -> Result<EasItem, WbxmlError> {
    let mut item = EasItem::default();
    for child in &item_el.children {
        match (child.page, child.token) {
            (PAGE_AIRSYNC, AS_SERVER_ID) => item.server_id = text_value(child)?,
            (PAGE_AIRSYNC, AS_APPLICATION_DATA) => {
                parse_application_data(child, &mut item);
            }
            _ => {}
        }
    }
    Ok(item)
}

/// Walk `ApplicationData` children and populate `EasItem` typed fields.
///
/// Dispatch is by `child.tag_name()` so the parser is robust to which code
/// page a tag was serialized on (EAS servers are inconsistent about whether
/// `From` lives on the Email page or is repeated on a child page). Unknown
/// tags are ignored — the MVP only surfaces the fields `EasItem` models.
///
/// Body type dispatch (MS-ASEMAIL `AirSyncBase:Body`):
///   - Type 2 → HTML  (`body_html`)
///   - Type 1 → plain (`body_text`)
///   - Type 4 → raw MIME BLOB (`body_mime`; [MS-ASCMD] §2.2.3.110.3) — its own slot, never
///     duplicated into the html/text slots
///   - other/missing → fallback writes the same payload to both slots so the UI degrades gracefully
///     rather than showing an empty message.
///
/// Flag: MS-ASEMAIL `Flag` has a `Status` child; `Status = "2"` means the
/// message is flagged for follow-up, so we set `flag = Some(true)` only in
/// that case (and `Some(false)` if a Flag element is present with any other
/// Status). Absent Flag → `None` (unknown).
pub fn parse_application_data(app_data: &WbxmlElement, item: &mut EasItem) {
    for child in &app_data.children {
        match child.tag_name() {
            "Subject" => item.subject = text_value_opt(child),
            "From" => item.from = text_value_opt(child),
            "To" => item.to = text_value_opt(child),
            "Cc" => item.cc = text_value_opt(child),
            "Bcc" => item.bcc = text_value_opt(child),
            "ReplyTo" => item.reply_to = text_value_opt(child),
            "DateReceived" => item.date_received = text_value_opt(child),
            "Read" => item.read = text_value_opt(child).map(|s| s == "1"),
            "Flag" => {
                // Flag.Status == "2" → active flag. Any other present Status
                // value is treated as not-flagged; absent Status is also
                // not-flagged. We only set Some(..) when a Flag element exists.
                let active = child
                    .children
                    .iter()
                    .any(|c| c.tag_name() == "Status" && text_value_opt(c).as_deref() == Some("2"));
                item.flag = Some(active);
            }
            "Importance" => item.importance = text_value_opt(child).and_then(|s| s.parse().ok()),
            "Body" => parse_body(child, item),
            "Attachments" => parse_attachments(child, item),
            "ConversationId" => {
                // ConversationId (Email2 page 22, token 0x09) is opaque binary
                // on the wire, but many Exchange deployments serialize it as
                // base64 *text*. Handle both variants and keep the bytes
                // verbatim — downstream treats `conversation_id` as opaque
                // bytes (no base64 decode). A missing or empty value must map
                // to `None` (not `Some(vec![])`), since empty != absent and
                // `Some([])` would serialize as `"conversationId":[]`,
                // misleading the frontend's threading logic.
                item.conversation_id = match &child.value {
                    WbxmlValue::Opaque(b) if !b.is_empty() => Some(b.clone()),
                    WbxmlValue::Text(s) if !s.is_empty() => Some(s.as_bytes().to_vec()),
                    _ => None,
                };
            }
            "IsDraft" => item.is_draft = text_value_opt(child).map(|s| s == "1"),
            // Task 4 (meeting requests): MessageClass distinguishes
            // invitations (IPM.Schedule.Meeting.Request) from ordinary mail
            // at a glance and gates the reading pane's meeting banner.
            "MessageClass" => item.message_class = text_value_opt(child),
            // [MS-ASEMAIL] §2.2.2.47 — numeric enum on the wire ("0".."6").
            // Non-numeric values are dropped to None (the UI then treats the
            // item as a non-respondable meeting message, never as a crash).
            "MeetingMessageType" => {
                let raw = text_value_opt(child);
                item.meeting_message_type = raw.as_deref().and_then(|s| match s.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        log::warn!(
                            "ApplicationData: malformed MeetingMessageType \"{s}\"; ignoring"
                        );
                        None
                    }
                });
            }
            // [MS-ASEMAIL] §2.2.2.48 — container of the meeting logistics
            // (children are Email-page tokens too; dispatch by tag name).
            "MeetingRequest" => item.meeting = Some(parse_meeting_request(child)),
            // Tags we deliberately ignore for MVP — they are either metadata
            // we don't model yet, or already consumed at a higher level
            // (e.g. Status on ApplicationData belongs to the Sync command,
            // not the item).
            "InternetCPID" | "ContentClass" | "ThreadTopic" | "Status" => {}
            _ => {} // unknown tags: ignore
        }
    }
}

/// Parse an `email:MeetingRequest` container ([MS-ASEMAIL] §2.2.2.48) into a
/// [`MeetingRequestInfo`]. Only the children the reading pane renders (plus
/// the calendar-identity key, M8 follow-up #4) are surfaced; the rest
/// (`DtStamp`, `Sensitivity`, `BusyStatus`, recurrence children) are ignored
/// until a consumer exists. Booleans follow the `Read` convention ("1" =
/// true); a present-but-unparseable numeric child maps to `None` rather than
/// aborting the item parse.
fn parse_meeting_request(elem: &WbxmlElement) -> MeetingRequestInfo {
    let mut info = MeetingRequestInfo::default();
    for child in &elem.children {
        match child.tag_name() {
            "StartTime" => info.start_time = text_value_opt(child),
            "EndTime" => info.end_time = text_value_opt(child),
            // ≤16.0 wire form ([MS-ASEMAIL] §2.2.2.48): plain-text leaf on
            // the Email page (2, 0x21) — the `_` arm also covers a gateway
            // sending the legacy Calendar-page leaf (4, 0x17).
            // 16.x wire form ([MS-ASWBXML] §2.1.2.1.5 note 2): an
            // AirSyncBase CONTAINER whose value is the DisplayName child —
            // the M8-L1 shape, shared with the Calendar parse.
            "Location" => match (child.page, child.token) {
                (pages::BASE, base::LOCATION) => {
                    info.location = parse_location_16x("email MeetingRequest", child)
                }
                _ => info.location = text_value_opt(child),
            },
            "Organizer" => info.organizer = text_value_opt(child),
            "ResponseRequested" => {
                info.response_requested = text_value_opt(child).map(|s| s == "1");
            }
            "AllDayEvent" => info.all_day_event = text_value_opt(child).map(|s| s == "1"),
            "InstanceType" => {
                let raw = text_value_opt(child);
                info.instance_type = raw.as_deref().and_then(|s| match s.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        log::warn!("MeetingRequest: malformed InstanceType \"{s}\"; ignoring");
                        None
                    }
                });
            }
            // M8 follow-up #4 ([MS-ASEMAIL] §3.1.4.7 / [MS-ASWBXML]
            // §2.1.2.1.4 note 4): at 16.0/16.1 the MeetingRequest carries
            // the CALENDAR-page UID tag (4, 0x28 → tag name "UID") verbatim
            // — the exact-key invite↔event correlation value, no conversion.
            // Whichever of UID/GlobalObjId appears later on the wire wins
            // (the file-wide convention); the two never coexist in practice
            // (the protocol version selects the element).
            "UID" => {
                info.uid = text_value_opt(child).filter(|s| !s.is_empty());
            }
            // ≤14.1 form: the base64 GlobalObjId, converted to the calendar
            // UID string space per §3.1.4.7 steps 1-5.
            "GlobalObjId" => {
                info.uid =
                    crate::meeting_uid::global_obj_id_to_uid(text_value_opt(child).as_deref());
            }
            _ => {}
        }
    }
    info
}

/// Parse an `AirSyncBase:Body` element into `body_html` / `body_text` /
/// `body_mime` / `body_truncated` / `preview` on the item.
fn parse_body(elem: &WbxmlElement, item: &mut EasItem) {
    let mut body_type: Option<u8> = None;
    let mut data: Option<String> = None;
    let mut truncated = false;
    let mut preview: Option<String> = None;
    for child in &elem.children {
        match child.tag_name() {
            "Type" => body_type = text_value_opt(child).and_then(|s| s.parse().ok()),
            "Data" => data = text_value_opt(child),
            "Truncated" => truncated = text_value_opt(child).as_deref() == Some("1"),
            "Preview" => preview = text_value_opt(child),
            "EstimatedDataSize" => {} // not surfaced on EasItem
            _ => {}
        }
    }
    match body_type {
        Some(2) => item.body_html = data, // Type 2 = HTML
        Some(1) => item.body_text = data, // Type 1 = PlainText
        // Type 4 = MIME BLOB ([MS-ASCMD] §2.2.3.110.3): the raw RFC 5322
        // message. Its own slot — do NOT also fill body_html/body_text; the
        // MIME parser (S/MIME / view-source work) is its only consumer.
        Some(4) => item.body_mime = data,
        _ => {
            // Unknown / missing type: write to both slots so the UI can still
            // render something. Prefer HTML for display, plain for search.
            item.body_html = data.clone();
            item.body_text = data;
        }
    }
    item.body_truncated = if truncated { Some(true) } else { None };
    item.preview = preview;
}

/// Parse an `AirSyncBase:Attachments` container into `item.attachments` and
/// set `has_attachments` based on whether any `Attachment` children were found.
fn parse_attachments(elem: &WbxmlElement, item: &mut EasItem) {
    for child in &elem.children {
        if child.tag_name() != "Attachment" {
            continue;
        }
        let mut att = EasAttachment::default();
        for field in &child.children {
            match field.tag_name() {
                "DisplayName" => att.display_name = text_value_opt(field).unwrap_or_default(),
                "FileReference" => att.file_reference = text_value_opt(field).unwrap_or_default(),
                "Method" => att.method = text_value_opt(field).and_then(|s| s.parse().ok()),
                "ContentId" => att.content_id = text_value_opt(field),
                "IsInline" => att.is_inline = text_value_opt(field).as_deref() == Some("1"),
                "ContentType" => att.content_type = text_value_opt(field),
                "EstimatedDataSize" => {
                    att.estimated_data_size = text_value_opt(field).and_then(|s| s.parse().ok());
                }
                "ContentLocation" => att.content_location = text_value_opt(field),
                _ => {}
            }
        }
        item.attachments.push(att);
    }
    item.has_attachments = !item.attachments.is_empty();
}

// ============================================================================
// Sync Change (client-to-server upsync)
// ============================================================================

/// One client-side item mutation carried by a Sync `Commands > Change`
/// element. `server_id` is the wire identifier (the message's `remote_id`
/// verbatim since M6.5 — the pre-M6.5 hashed-uid / `eas_uid_map` bridge is
/// retired). `read` maps to `email:Read` (0/1);
/// `starred` maps to `email:Flag` — `Some(true)` emits the full task-like
/// Flag container (Status "2", FlagType "FollowUp", tasks-page start/due
/// dates), `Some(false)` an empty `<Flag/>`, `None` no Flag element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EasChange {
    pub server_id: String,
    pub read: Option<bool>,
    pub starred: Option<bool>,
}

/// One client-side Calendar item mutation carried by a Sync Commands
/// request (the upsync direction of [MS-ASSYNC] §2.2.2). OUR vocabulary
/// maps onto the wire commands that act on an existing item:
///
/// - `Add` → wire `airsync:Add` { ClientId, ApplicationData } — the item has no ServerId yet; the
///   server correlates the response through the ClientId.
/// - `Replace` → wire `airsync:Change` carrying ServerId ([MS-ASSYNC] §2.2.2 — the Change command
///   updates an existing item). "Replace" is OUR client-side vocabulary only; there is no wire
///   Replace command.
/// - `Remove` → wire `airsync:Delete` { ServerId } — the server's soft-delete semantics; acceptable
///   for v1 per the M8 design (D1).
#[derive(Debug, Clone, PartialEq)]
pub enum CalendarChange {
    /// Create a new event in the collection.
    Add {
        /// Client-generated correlation id (≤ 40 chars, [MS-ASCMD];
        /// Exchange 15.2 rejects over-cap ids with in-body Status 103 —
        /// task-11 live evidence). Synthesize with
        /// [`new_calendar_client_id`], which guarantees the cap.
        client_id: String,
        /// The event payload, serialized via
        /// [`build_calendar_application_data`] (M8 Task 1).
        props: CalendarEventWrite,
    },
    /// Update an existing event (wire: `airsync:Change` with ServerId).
    Replace {
        /// Wire identifier of the existing item.
        server_id: String,
        /// The event payload, serialized via
        /// [`build_calendar_application_data`] (M8 Task 1).
        props: CalendarEventWrite,
    },
    /// Delete an existing event (wire: `airsync:Delete` with ServerId).
    Remove {
        /// Wire identifier of the item to delete.
        server_id: String,
    },
}

/// Outcome of a Sync command that carried client-side `Commands` (the upsync
/// direction). Beyond the rotated `new_key` and the collection `status`, the
/// response Collection MAY itself carry server-side `Commands` ([MS-ASSYNC]
/// §2.2.2 — the server piggybacks pending changes onto the upsync response).
/// Those are surfaced here via the same `parse_item` path the downsync uses;
/// discarding them while adopting the rotated key would silently diverge from
/// the server. Empty when the response carries no Commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncChangeOutcome {
    pub new_key: String,
    /// Collection status (MS-ASSYNC §2.2.3.23); 1 = success.
    pub status: u32,
    pub piggybacked_added: Vec<EasItem>,
    pub piggybacked_updated: Vec<EasItem>,
    pub piggybacked_deleted: Vec<String>,
    /// Per-item Add acknowledgements from the response Collection's
    /// `Responses` element ([MS-ASCMD] §2.2.3.154): the server echoes each
    /// client Add as `Add { ClientId, ServerId?, Status }` (§2.2.3.7.2),
    /// mapping the request's ClientId to the ServerId it assigned. Per
    /// §2.2.3.154 acks are only sent for SUCCESSFUL additions — an Add with
    /// no ack here means success with no id to correlate. Empty when the
    /// response carries no Responses element (the email-upsync shape).
    pub add_acks: Vec<CalendarAddAck>,
    /// Per-item statuses for client Change/Delete commands, from the same
    /// `Responses` element (§2.2.3.24 Change / §2.2.3.42.2 Delete). Per
    /// §2.2.3.154 the server only sends these for FAILED changes and
    /// deletions — absence means success. Empty when the response carries no
    /// Responses element.
    pub item_statuses: Vec<CalendarItemStatus>,
}

impl SyncChangeOutcome {
    /// True when the response carried no server-side Commands (the common case).
    pub fn has_piggybacked(&self) -> bool {
        !(self.piggybacked_added.is_empty()
            && self.piggybacked_updated.is_empty()
            && self.piggybacked_deleted.is_empty())
    }
}

/// Per-item acknowledgement of one client Add, echoed by the server under
/// the response Collection's `Responses` element ([MS-ASCMD] §2.2.3.7.2:
/// "The server then responds with an Add element in a Responses element,
/// which specifies the client ID and the server ID that was assigned to the
/// new item" — the §4.5.3.2 example shape `{ ClientId, ServerId, Status }`).
/// Named for its first consumer, the M8 calendar upsync engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarAddAck {
    /// The request's correlation id (ClientId), echoed verbatim — the key
    /// the caller uses to find its pending add.
    pub client_id: String,
    /// Per-item Status ([MS-ASCMD] §2.2.3.177.17); 1 = success. The raw
    /// value is preserved for the engine's failure-class machinery — deep
    /// retry/discard classification lives in the engine, NOT here.
    /// Item-scoped codes verifiable in docs/Exchange/mscmd.txt §2.2.3.177.17:
    /// 6 = "Error in client/server conversion" (malformed/invalid item —
    /// NOT transient, "stop sending the item"), 8 = "Object not found"
    /// (the CollectionId/ServerId is no longer valid).
    pub status: u32,
    /// The ServerId the server assigned to the new item. `None` when the
    /// Add failed (status != 1) or the element is absent — the server only
    /// assigns an id on success.
    pub server_id: Option<String>,
}

impl CalendarAddAck {
    /// True when the per-item Status is 1 (success) per [MS-ASCMD]
    /// §2.2.3.177.17. Note the surrounding contract of §2.2.3.154: "the
    /// client only receives responses for successful additions … and failed
    /// changes and deletions. When the client does not receive a response,
    /// the client MUST assume that the operation succeeded" — an Add with
    /// NO ack at all also means success; there is simply no id to persist.
    pub fn success(&self) -> bool {
        self.status == 1
    }
}

/// Which client command a `Responses` item answers ([MS-ASCMD] §2.2.3.154:
/// each response "is wrapped in an element with the same name as the
/// operation").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseItemKind {
    /// Wire `airsync:Change` ([MS-ASCMD] §2.2.3.24) — answers OUR
    /// [`CalendarChange::Replace`].
    Change,
    /// Wire `airsync:Delete` ([MS-ASCMD] §2.2.3.42.2) — answers OUR
    /// [`CalendarChange::Remove`].
    Delete,
}

/// Per-item status of one client Change or Delete command, echoed under the
/// response Collection's `Responses` element ([MS-ASCMD] §2.2.3.24 Change
/// (Sync) / §2.2.3.42.2 Delete (Sync): `{ ServerId, Status }`). Delete
/// responses are rare on the wire — per §2.2.3.154 the server acks
/// deletions only when they FAIL — so the parser surfaces exactly what
/// arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarItemStatus {
    /// The wire identifier of the item the status answers.
    pub server_id: String,
    /// Per-item Status ([MS-ASCMD] §2.2.3.177.17); 1 = success. Raw value
    /// preserved for the engine's failure-class machinery — deep
    /// retry/discard classification lives in the engine, NOT here (see
    /// [`CalendarAddAck::status`] for the citable item-scoped codes).
    pub status: u32,
    /// Whether this status answers a Change or a Delete.
    pub kind: ResponseItemKind,
}

impl CalendarItemStatus {
    /// True when the per-item Status is 1 (success) per [MS-ASCMD]
    /// §2.2.3.177.17. Per §2.2.3.154 these items are only sent for FAILED
    /// changes and deletions, so `false` here is the actionable case; a
    /// command with NO item status at all also means success.
    pub fn success(&self) -> bool {
        self.status == 1
    }
}

// ---------- Email (page 2) Flag tag ids ([MS-ASWBXML] §2.1.2.1.3) ----------
// `Flag` itself lives in `tags::email::FLAG` (0x3A); its children are not in
// tags.rs, so they are local constants here.
const EMAIL_FLAG_STATUS: u8 = 0x3B; // "Status" child of Flag — "2" = flagged
const EMAIL_FLAG_TYPE: u8 = 0x3D; // "FlagType" — "FollowUp" is the standard value

// ---------- Tasks (page 9) tag ids used inside email:Flag ----------
// Per [MS-ASWBXML] §2.1.2.1.10 and Android EasSync.java:295-315, an active
// email Flag must carry Start/UtcStart/Due/UtcDue dates from the Tasks page —
// the container switches code page email(2) → tasks(9) mid-stream.
const PAGE_TASKS: u8 = 9;
const TASK_DUE_DATE: u8 = 0x0C;
const TASK_UTC_DUE_DATE: u8 = 0x0D;
const TASK_START_DATE: u8 = 0x1E;
const TASK_UTC_START_DATE: u8 = 0x1F;

/// Active flags get a due date one week out (Android `DateUtils.WEEK_IN_MILLIS`).
const FLAG_DUE_OFFSET_SECS: u64 = 7 * 24 * 60 * 60;

/// Build a Sync request carrying client-side `Commands > Change` elements
/// (the upsync direction of [MS-ASSYNC] §2.2.2).
///
/// WBXML shape:
/// ```xml
/// <Sync>
///   <Collections>
///     <Collection>
///       <SyncKey>{sync_key}</SyncKey>
///       <CollectionId>{collection_id}</CollectionId>
///       <Commands>
///         <Change>
///           <ServerId>{server_id}</ServerId>
///           <ApplicationData>
///             <email:Read>1</email:Read>   <!-- only when change.read is Some -->
///             <email:Flag>…</email:Flag>   <!-- only when change.starred is Some -->
///           </ApplicationData>
///         </Change>
///         …
///       </Commands>
///     </Collection>
///   </Collections>
/// </Sync>
/// ```
///
/// Same element gates as `build_sync_request`: NO `airsync:Class` (14.0+
/// rejects it — CollectionId identifies the collection) and NO `GetChanges`
/// (invalid in 16.1). `ApplicationData` is always emitted (schema-required
/// for a client Change). This wrapper stamps Flag dates from the wall clock;
/// tests use `build_sync_change_request_at` to pin the instant.
pub fn build_sync_change_request(
    collection_id: &str,
    sync_key: &str,
    changes: &[EasChange],
) -> WbxmlElement {
    build_sync_change_request_at(
        collection_id,
        sync_key,
        changes,
        std::time::SystemTime::now(),
    )
}

/// `build_sync_change_request` with an injectable clock for the Flag dates.
///
/// Flag emission (Android EasSync.java:295-315):
/// - `starred: Some(true)` → full container: `email:Flag > email:Status "2" + email:FlagType
///   "FollowUp"
///   + tasks:StartDate/UtcStartDate = now UTC
///   + tasks:DueDate/UtcDueDate = now + 7 days UTC`
///   (dates ISO-8601 `yyyy-MM-dd'T'HH:mm:ss.fff'Z'`). The tasks-page date
///     elements switch the code page email(2) → tasks(9) mid-container.
/// - `starred: Some(false)` → an empty `<email:Flag/>` element (no children).
/// - `starred: None` → no Flag element.
pub fn build_sync_change_request_at(
    collection_id: &str,
    sync_key: &str,
    changes: &[EasChange],
    now: std::time::SystemTime,
) -> WbxmlElement {
    let change_elements: Vec<WbxmlElement> = changes
        .iter()
        .map(|change| {
            let mut app_data_children = Vec::new();
            if let Some(read) = change.read {
                app_data_children.push(WbxmlElement::text(
                    tags::email::PAGE,
                    tags::email::READ,
                    if read { "1" } else { "0" },
                ));
            }
            if let Some(starred) = change.starred {
                if starred {
                    let start = format_eas_datetime_utc(now);
                    let due = format_eas_datetime_utc(
                        now + std::time::Duration::from_secs(FLAG_DUE_OFFSET_SECS),
                    );
                    app_data_children.push(WbxmlElement::container(
                        tags::email::PAGE,
                        tags::email::FLAG,
                        vec![
                            WbxmlElement::text(tags::email::PAGE, EMAIL_FLAG_STATUS, "2"),
                            WbxmlElement::text(tags::email::PAGE, EMAIL_FLAG_TYPE, "FollowUp"),
                            WbxmlElement::text(PAGE_TASKS, TASK_START_DATE, start.clone()),
                            WbxmlElement::text(PAGE_TASKS, TASK_UTC_START_DATE, start),
                            WbxmlElement::text(PAGE_TASKS, TASK_DUE_DATE, due.clone()),
                            WbxmlElement::text(PAGE_TASKS, TASK_UTC_DUE_DATE, due),
                        ],
                    ));
                } else {
                    // Clearing a flag is an empty <Flag/> element (Android's
                    // `s.tag(Tags.EMAIL_FLAG)`) — no children, no dates.
                    app_data_children
                        .push(WbxmlElement::empty(tags::email::PAGE, tags::email::FLAG));
                }
            }
            WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_CHANGE,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, change.server_id.clone()),
                    WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, app_data_children),
                ],
            )
        })
        .collect();

    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, sync_key),
            WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, collection_id),
            WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, change_elements),
        ],
    );

    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}

/// Build a Sync request carrying client-side Calendar `Commands` (the
/// upsync direction of [MS-ASSYNC] §2.2.2) — the Calendar twin of
/// [`build_sync_change_request`].
///
/// WBXML shape (see [`CalendarChange`] for the OUR-vocabulary → wire
/// mapping):
/// ```xml
/// <Sync>
///   <Collections>
///     <Collection>
///       <SyncKey>{sync_key}</SyncKey>
///       <CollectionId>{collection_id}</CollectionId>
///       <Commands>
///         <Add>                                    <!-- CalendarChange::Add -->
///           <ClientId>{client_id}</ClientId>
///           <ApplicationData>calendar:Timezone, … (M8 Task 1)</ApplicationData>
///         </Add>
///         <Change>                                 <!-- CalendarChange::Replace -->
///           <ServerId>{server_id}</ServerId>
///           <ApplicationData>…</ApplicationData>
///         </Change>
///         <Delete>                                 <!-- CalendarChange::Remove -->
///           <ServerId>{server_id}</ServerId>
///         </Delete>
///       </Commands>
///     </Collection>
///   </Collections>
/// </Sync>
/// ```
///
/// - `ApplicationData` is [`build_calendar_application_data`]'s output VERBATIM — this builder adds
///   no calendar properties.
/// - Same element gates as the email builder: NO `airsync:Class` (14.0+ rejects it — CollectionId
///   identifies the collection) and NO `GetChanges` (invalid in 16.1).
/// - Infallible like the email precedent: callers run [`CalendarEventWrite::validate`] first, and
///   supply the Add `client_id` themselves (synthesize with [`new_calendar_client_id`], which
///   guarantees the [MS-ASCMD] 40-char cap) — the builder never synthesizes or clamps ids.
pub fn build_calendar_change_request(
    collection_id: &str,
    sync_key: &str,
    changes: &[CalendarChange],
    protocol_version: &str,
) -> WbxmlElement {
    let command_elements: Vec<WbxmlElement> = changes
        .iter()
        .map(|change| match change {
            // Add: ClientId + ApplicationData. The added item has no
            // ServerId yet — the server correlates its response (and the
            // new ServerId) through the ClientId.
            CalendarChange::Add { client_id, props } => WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_ADD,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_CLIENT_ID, client_id.clone()),
                    build_calendar_application_data(props, protocol_version),
                ],
            ),
            // Replace → wire Change ([MS-ASSYNC] §2.2.2): ServerId +
            // ApplicationData, the same envelope shape the email builder
            // emits for its Change commands.
            CalendarChange::Replace { server_id, props } => WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_CHANGE,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, server_id.clone()),
                    build_calendar_application_data(props, protocol_version),
                ],
            ),
            // Remove → wire Delete ([MS-ASSYNC] §2.2.2.4): a CONTAINER whose
            // ServerId is a child element ([MS-ASCMD] §2.2.3.42.2), with no
            // ApplicationData.
            CalendarChange::Remove { server_id } => WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_DELETE,
                vec![WbxmlElement::text(
                    PAGE_AIRSYNC,
                    AS_SERVER_ID,
                    server_id.clone(),
                )],
            ),
        })
        .collect();

    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, sync_key),
            WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, collection_id),
            WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, command_elements),
        ],
    );

    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}

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
            (PAGE_AIRSYNC, AS_CHANGE) | (PAGE_AIRSYNC, AS_DELETE) => {
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
                                        "Sync Responses: malformed {:?} Status \"{s}\"; \
                                         keeping the default of 1",
                                        kind
                                    ),
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if server_id.is_empty() {
                    log::warn!(
                        "Sync Responses: {:?} response without ServerId — the status cannot be \
                         correlated, skipping",
                        kind
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

// ============================================================================
// GetItemEstimate
// ============================================================================

/// Build a GetItemEstimate request.
///
/// Wire shape per [MS-ASWBXML] §2.1.2.1.7 (code page 6) and the [MS-ASCMD]
/// §6.20 request schema, 14.0+ form A:
/// ```xml
/// <GetItemEstimate>                          <!-- page 6, 0x05 -->
///   <Collections>                            <!-- page 6, 0x07 -->
///     <Collection>                           <!-- page 6, 0x08 -->
///       <airsync:SyncKey>{sync_key}</>       <!-- PAGE 0, 0x0B — not page 6 -->
///       <CollectionId>{collection_id}</>     <!-- page 6, 0x0A (0x0C is Estimate!) -->
///       <airsync:Options>                    <!-- PAGE 0, 0x17; only when filtering -->
///         <airsync:FilterType>{days}</>      <!-- PAGE 0, 0x18 — not page 6 -->
///       </airsync:Options>
///     </Collection>
///   </Collections>
/// </GetItemEstimate>
/// ```
/// Notes:
/// - SyncKey / FilterType are AirSync-page (0) tokens even inside a GetItemEstimate request
///   ([MS-ASCMD] §2.2.1.9 element table prefixes them `airsync:`; the page-6 token table has no
///   SyncKey/FilterType at all).
/// - There is no top-level `Class` element in the 14.0+ request form. The page-6 Class token (0x09)
///   is 2.5/12.x-only per [MS-ASWBXML] §2.1.2.1.7 note 1, and the §6.20 form-A sequence has no
///   Class slot — so `req.class` is intentionally not emitted (the client negotiates 16.1).
/// - FilterType 0 means "all items" (the server default), so the whole Options element is omitted
///   when `filter_age_days == 0`.
pub fn build_get_item_estimate_request(req: &GetItemEstimateRequest) -> WbxmlElement {
    pub const PAGE_GIE: u8 = 6;
    pub const GIE_GET_ITEM_ESTIMATE: u8 = 0x05;
    pub const GIE_COLLECTIONS: u8 = 0x07;
    pub const GIE_COLLECTION: u8 = 0x08;
    pub const GIE_COLLECTION_ID: u8 = 0x0A;

    let mut collection_children = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, req.sync_key.clone()),
        WbxmlElement::text(PAGE_GIE, GIE_COLLECTION_ID, req.collection_id.clone()),
    ];
    if req.filter_age_days != 0 {
        collection_children.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_OPTIONS,
            vec![WbxmlElement::text(
                PAGE_AIRSYNC,
                AS_FILTER_TYPE,
                req.filter_age_days.to_string(),
            )],
        ));
    }

    let collection = WbxmlElement::container(PAGE_GIE, GIE_COLLECTION, collection_children);

    WbxmlElement::container(
        PAGE_GIE,
        GIE_GET_ITEM_ESTIMATE,
        vec![WbxmlElement::container(
            PAGE_GIE,
            GIE_COLLECTIONS,
            vec![collection],
        )],
    )
}

/// Parse a GetItemEstimate response per [MS-ASCMD] §6.21:
/// ```xml
/// <GetItemEstimate>            <!-- page 6, 0x05 -->
///   <Response>                 <!-- page 6, 0x0D (NOT 0x06) -->
///     <Status>1</Status>       <!-- page 6, 0x0E — sibling of Collection -->
///     <Collection>             <!-- page 6, 0x08 -->
///       <CollectionId>..</>    <!-- page 6, 0x0A -->
///       <Estimate>42</>        <!-- page 6, 0x0C -->
///     </Collection>
///   </Response>
/// </GetItemEstimate>
/// ```
/// The Response-level `<Status>` is surfaced on `result.status` (1 = success,
/// 3 = sync state not primed); an absent Status defaults to 1. Live evidence
/// 2026-08-02: Exchange 2019 answers Status 3 for a collection that has never
/// been Synced — callers must read it, not assume count-0 means "up to date".
pub fn parse_get_item_estimate_response(
    root: &WbxmlElement,
) -> Result<GetItemEstimateResult, WbxmlError> {
    pub const PAGE_GIE: u8 = 6;
    pub const GIE_RESPONSE: u8 = 0x0D;
    pub const GIE_STATUS: u8 = 0x0E;
    pub const GIE_COLLECTION: u8 = 0x08;
    pub const GIE_COLLECTION_ID: u8 = 0x0A;
    pub const GIE_ESTIMATE: u8 = 0x0C;

    let mut result = GetItemEstimateResult {
        status: 1, // success default per [MS-ASCMD] §6.21 when Status absent
        ..GetItemEstimateResult::default()
    };
    for child in &root.children {
        if child.page == PAGE_GIE && child.token == GIE_RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_GIE && resp_child.token == GIE_STATUS {
                    let s = text_value(resp_child).unwrap_or_default();
                    result.status = s.parse().unwrap_or(1);
                } else if resp_child.page == PAGE_GIE && resp_child.token == GIE_COLLECTION {
                    for col_child in &resp_child.children {
                        match (col_child.page, col_child.token) {
                            (PAGE_GIE, GIE_COLLECTION_ID) => {
                                result.collection_id = text_value(col_child).unwrap_or_default();
                            }
                            (PAGE_GIE, GIE_ESTIMATE) => {
                                let s = text_value(col_child).unwrap_or("0".to_string());
                                result.count = s.parse().unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

// ============================================================================
// Tests — M8 Task 4: class-aware SyncResult seam
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{
            CAL_ALL_DAY_EVENT, CAL_BUSY_STATUS, CAL_END_TIME, CAL_START_TIME, CAL_SUBJECT,
            CalendarAttendee, CalendarEventProps, CalendarException, CalendarRecurrence,
            PAGE_CALENDAR, TimeZoneBlob, TziTimeZone,
            tests::{TZI_FLAT_UTC8, fixture_full_app_data},
        },
        contacts::{CON_FILE_AS, ContactsContactProps, PAGE_CONTACTS},
        contacts_testutil::{expected_full_contact_props, fixture_full_contact_app_data},
    };

    /// All-day Calendar ApplicationData (shape of the Task-2
    /// `parse_all_day_item` fixture). Deliberately DIFFERENT from the full
    /// fixture so a crossed wire between the Add and Change items cannot
    /// hide behind equal props.
    fn all_day_app_data() -> WbxmlElement {
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Company Holiday"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "1"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "20260820T000000Z"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260821T000000Z"),
                WbxmlElement::text(PAGE_CALENDAR, CAL_BUSY_STATUS, "0"),
            ],
        )
    }

    /// Golden props for [`fixture_full_app_data`] — mirrors the
    /// `parse_full_core_item` assertion in calendar.rs so the seam test
    /// locks the FULL props fidelity end-to-end through the Sync envelope.
    fn expected_full_props() -> CalendarEventProps {
        CalendarEventProps {
            all_day_event: false,
            start_time: Some("20260818T090000Z".to_string()),
            end_time: Some("20260818T100000Z".to_string()),
            dtstamp: Some("20260815T120000Z".to_string()),
            subject: Some("Weekly Sync".to_string()),
            location: Some("Room 42".to_string()),
            body_plain: Some("Agenda: sync status".to_string()),
            organizer_name: Some("Felix Zhou".to_string()),
            organizer_email: Some("felixzhou@kylins.local".to_string()),
            sensitivity: Some(2),
            busy_status: Some(2),
            reminder_set: true,
            reminder_minutes: Some(15),
            meeting_status: Some(1),
            response_requested: true,
            uid: None,
            time_zone: Some(TimeZoneBlob {
                raw_base64: Some(TZI_FLAT_UTC8.to_string()),
                parsed: Some(TziTimeZone {
                    base_bias_minutes: -480,
                    standard: None,
                    daylight: None,
                }),
            }),
            recurrence: Some(CalendarRecurrence {
                recurrence_type: 1,
                interval: Some(1),
                day_of_week: Some(62),
                until: Some("20261225T090000Z".to_string()),
                no_end: false,
                ..Default::default()
            }),
            exceptions: vec![
                CalendarException {
                    deleted: true,
                    exception_start_time: Some("20260825T090000Z".to_string()),
                    ..Default::default()
                },
                CalendarException {
                    deleted: false,
                    exception_start_time: Some("20260901T090000Z".to_string()),
                    start_time: Some("20260901T100000Z".to_string()),
                    end_time: Some("20260901T110000Z".to_string()),
                    subject: Some("Moved".to_string()),
                    location: Some("Room 7".to_string()),
                    body_plain: None,
                    // The fixture carries AllDayEvent "0" → Some(false)
                    // (interlude-A tri-state; absence parses to None).
                    all_day_event: Some(false),
                },
            ],
            attendees: vec![
                CalendarAttendee {
                    name: Some("Bob".to_string()),
                    email: "bob@example.com".to_string(),
                    status: Some(3),
                },
                CalendarAttendee {
                    name: Some("Carol".to_string()),
                    email: "carol@example.com".to_string(),
                    status: None,
                },
            ],
        }
    }

    /// Calendar-class Sync response fixture: Collection with SyncKey
    /// "{cal-sk1}", Status "1", MoreAvailable, and a Commands block with
    /// one Add (ServerId "cal:1" + the Task-2/3 FULL ApplicationData
    /// fixture), one Change (ServerId "cal:2" + the all-day fixture), one
    /// Delete (ServerId "cal:3" — deletes are class-agnostic on the wire).
    fn fixture_calendar_sync_response() -> WbxmlElement {
        let add = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_ADD,
            vec![
                WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "cal:1"),
                fixture_full_app_data(),
            ],
        );
        let change = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_CHANGE,
            vec![
                WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "cal:2"),
                all_day_app_data(),
            ],
        );
        // MS-ASCMD 2.2.3.42.2: Delete is a CONTAINER with the ServerId as
        // a child — same envelope shape as Add/Change.
        let delete = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_DELETE,
            vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "cal:3")],
        );
        let commands =
            WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![add, change, delete]);
        let collection = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTION,
            vec![
                WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{cal-sk1}"),
                WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE),
                commands,
            ],
        );
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_SYNC,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTIONS,
                vec![collection],
            )],
        )
    }

    /// Brief test (1): a Calendar-class response routes Add/Change through
    /// the MS-ASCAL parser — `calendar_added` / `calendar_updated` carry
    /// the ServerIds + FULL props, deletes land in the shared
    /// `deleted_server_ids`, and the Email-shaped `added` / `updated`
    /// vectors stay EMPTY.
    #[test]
    fn calendar_class_sync_routes_items_to_calendar_vectors() {
        let tree = fixture_calendar_sync_response();

        let result = parse_sync_response_for_class(&tree, "Calendar").expect("parse");

        // Envelope fields parse identically for the Calendar class.
        assert_eq!(result.sync_key, "{cal-sk1}");
        assert_eq!(result.status, 1);
        assert!(result.more_available);

        // Email-shaped vectors stay empty — no double delivery.
        assert!(
            result.added.is_empty(),
            "Calendar class must not fill added"
        );
        assert!(
            result.updated.is_empty(),
            "Calendar class must not fill updated"
        );

        // Add → calendar_added with ServerId + full props.
        assert_eq!(result.calendar_added.len(), 1, "exactly one Add command");
        let added = &result.calendar_added[0];
        assert_eq!(added.server_id, "cal:1");
        assert_eq!(
            added.props,
            expected_full_props(),
            "full Task-2/3 props fidelity through the Sync envelope"
        );

        // Change → calendar_updated with ServerId + the all-day props.
        assert_eq!(
            result.calendar_updated.len(),
            1,
            "exactly one Change command"
        );
        let updated = &result.calendar_updated[0];
        assert_eq!(updated.server_id, "cal:2");
        assert!(updated.props.all_day_event);
        assert_eq!(updated.props.subject.as_deref(), Some("Company Holiday"));
        assert_eq!(
            updated.props.start_time.as_deref(),
            Some("20260820T000000Z")
        );
        assert_eq!(updated.props.end_time.as_deref(), Some("20260821T000000Z"));
        assert_eq!(updated.props.busy_status, Some(0));

        // Delete → the shared, class-agnostic deleted_server_ids.
        assert_eq!(result.deleted_server_ids, vec!["cal:3".to_string()]);
    }

    /// Brief test (2): the SAME response under class "Email" keeps today's
    /// Email-shaped behavior bit-for-bit — calendar vectors empty, items in
    /// `added` / `updated` via the tag_name-dispatching Email parser (which
    /// picks up the page-4 `Subject` token collision and the AirSyncBase
    /// Type-1 Body, and ignores the rest).
    #[test]
    fn email_class_sync_keeps_email_shaped_parse() {
        let tree = fixture_calendar_sync_response();

        let result = parse_sync_response_for_class(&tree, "Email").expect("parse");

        assert!(
            result.calendar_added.is_empty(),
            "Email class must not fill calendar_added"
        );
        assert!(
            result.calendar_updated.is_empty(),
            "Email class must not fill calendar_updated"
        );

        // Email-shaped items, unchanged from the pre-M8 parse path.
        assert_eq!(result.added.len(), 1);
        assert_eq!(result.added[0].server_id, "cal:1");
        assert_eq!(result.added[0].subject.as_deref(), Some("Weekly Sync"));
        assert_eq!(
            result.added[0].body_text.as_deref(),
            Some("Agenda: sync status"),
            "AirSyncBase Body Type=1 lands in body_text on the Email path"
        );
        assert_eq!(result.added[0].body_html, None);
        assert_eq!(result.added[0].from, None);

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].server_id, "cal:2");
        assert_eq!(
            result.updated[0].subject.as_deref(),
            Some("Company Holiday")
        );

        assert_eq!(result.deleted_server_ids, vec!["cal:3".to_string()]);
    }

    /// Brief test (2b): an EMPTY class — the pre-M8 construction default —
    /// behaves exactly like "Email" (old behavior bit-for-bit).
    #[test]
    fn empty_class_sync_defaults_to_email_shaped_parse() {
        let tree = fixture_calendar_sync_response();

        let result = parse_sync_response_for_class(&tree, "").expect("parse");

        assert!(result.calendar_added.is_empty());
        assert!(result.calendar_updated.is_empty());
        assert_eq!(result.added.len(), 1);
        assert_eq!(result.added[0].server_id, "cal:1");
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.deleted_server_ids, vec!["cal:3".to_string()]);
    }

    /// Brief test (3): sync_key / more_available / status parse identically
    /// for both classes, and the legacy class-unaware entry matches the
    /// Email-class entry.
    #[test]
    fn sync_envelope_fields_identical_across_classes() {
        let tree = fixture_calendar_sync_response();

        let calendar = parse_sync_response_for_class(&tree, "Calendar").expect("parse");
        let email = parse_sync_response_for_class(&tree, "Email").expect("parse");
        let legacy = parse_sync_response(&tree).expect("parse");

        assert_eq!(calendar.sync_key, email.sync_key);
        assert_eq!(calendar.status, email.status);
        assert_eq!(calendar.more_available, email.more_available);
        assert_eq!(calendar.deleted_server_ids, email.deleted_server_ids);

        // The class-unaware entry is the Email-shaped entry.
        assert_eq!(legacy.sync_key, email.sync_key);
        assert_eq!(legacy.status, email.status);
        assert_eq!(legacy.added.len(), email.added.len());
        assert_eq!(legacy.updated.len(), email.updated.len());
    }

    // ========================================================================
    // Tests — M8-C task 1: Contacts-class SyncResult seam
    // ========================================================================

    /// Minimal Contacts ApplicationData for the Change item: FileAs only.
    /// Deliberately DIFFERENT from the full fixture so a crossed wire
    /// between the Add and Change items cannot hide behind equal props.
    fn file_as_only_app_data() -> WbxmlElement {
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(
                PAGE_CONTACTS,
                CON_FILE_AS,
                "Kerry, Anat",
            )],
        )
    }

    /// Contacts-class Sync response fixture: Collection with SyncKey
    /// "{con-sk1}", Status "1", MoreAvailable, and a Commands block with
    /// one Add (ServerId "con:1" + the full C1 ApplicationData fixture),
    /// one Change (ServerId "con:2" + the FileAs-only item), one Delete
    /// (ServerId "con:3" — deletes are class-agnostic on the wire).
    fn fixture_contacts_sync_response() -> WbxmlElement {
        let add = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_ADD,
            vec![
                WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "con:1"),
                fixture_full_contact_app_data(),
            ],
        );
        let change = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_CHANGE,
            vec![
                WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "con:2"),
                file_as_only_app_data(),
            ],
        );
        // MS-ASCMD 2.2.3.42.2: Delete is a CONTAINER with the ServerId as
        // a child — same envelope shape as Add/Change.
        let delete = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_DELETE,
            vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "con:3")],
        );
        let commands =
            WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![add, change, delete]);
        let collection = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTION,
            vec![
                WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{con-sk1}"),
                WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE),
                commands,
            ],
        );
        WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_SYNC,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTIONS,
                vec![collection],
            )],
        )
    }

    /// Brief test: a Contacts-class response routes Add/Change through the
    /// MS-ASCNTC parser — `contacts_added` / `contacts_updated` carry the
    /// ServerIds + FULL props, deletes land in the shared
    /// `deleted_server_ids`, and BOTH the Email-shaped `added` / `updated`
    /// and the Calendar vectors stay EMPTY (no double delivery — the
    /// regression pins for the other two classes).
    #[test]
    fn contacts_class_sync_routes_items_to_contacts_vectors() {
        let tree = fixture_contacts_sync_response();

        let result = parse_sync_response_for_class(&tree, "Contacts").expect("parse");

        // Envelope fields parse identically for the Contacts class.
        assert_eq!(result.sync_key, "{con-sk1}");
        assert_eq!(result.status, 1);
        assert!(result.more_available);

        // Email-shaped vectors stay empty — no double delivery.
        assert!(
            result.added.is_empty(),
            "Contacts class must not fill added"
        );
        assert!(
            result.updated.is_empty(),
            "Contacts class must not fill updated"
        );
        // Calendar vectors stay empty too — class routing is exclusive.
        assert!(
            result.calendar_added.is_empty(),
            "Contacts class must not fill calendar_added"
        );
        assert!(
            result.calendar_updated.is_empty(),
            "Contacts class must not fill calendar_updated"
        );

        // Add → contacts_added with ServerId + full C1 props.
        assert_eq!(result.contacts_added.len(), 1, "exactly one Add command");
        let added = &result.contacts_added[0];
        assert_eq!(added.server_id, "con:1");
        assert_eq!(
            added.props,
            expected_full_contact_props(),
            "full C1 props fidelity through the Sync envelope"
        );

        // Change → contacts_updated with ServerId + the FileAs-only props.
        assert_eq!(
            result.contacts_updated.len(),
            1,
            "exactly one Change command"
        );
        let updated = &result.contacts_updated[0];
        assert_eq!(updated.server_id, "con:2");
        assert_eq!(
            updated.props,
            ContactsContactProps {
                file_as: Some("Kerry, Anat".to_string()),
                ..Default::default()
            },
            "everything but FileAs stays None on the minimal item"
        );

        // Delete → the shared, class-agnostic deleted_server_ids.
        assert_eq!(result.deleted_server_ids, vec!["con:3".to_string()]);
    }

    /// Brief test: the SAME Contacts-class response under class "Email"
    /// keeps today's Email-shaped behavior bit-for-bit — contacts/calendar
    /// vectors empty, items in `added` / `updated` via the tag_name-
    /// dispatching Email parser (which ignores the page-1 contacts tokens
    /// and picks up only the AirSyncBase Type-1 Body).
    #[test]
    fn email_class_sync_keeps_email_shaped_parse_for_contacts_fixture() {
        let tree = fixture_contacts_sync_response();

        let result = parse_sync_response_for_class(&tree, "Email").expect("parse");

        assert!(
            result.contacts_added.is_empty(),
            "Email class must not fill contacts_added"
        );
        assert!(
            result.contacts_updated.is_empty(),
            "Email class must not fill contacts_updated"
        );
        assert!(result.calendar_added.is_empty());
        assert!(result.calendar_updated.is_empty());

        // Email-shaped items, unchanged from the pre-M8 parse path.
        assert_eq!(result.added.len(), 1);
        assert_eq!(result.added[0].server_id, "con:1");
        assert_eq!(
            result.added[0].body_text.as_deref(),
            Some("Prefers plain-text bodies."),
            "AirSyncBase Body Type=1 lands in body_text on the Email path"
        );
        assert_eq!(
            result.added[0].subject, None,
            "page-1 contacts tokens are invisible to the Email parser"
        );
        assert_eq!(result.added[0].from, None);

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].server_id, "con:2");
        assert_eq!(result.updated[0].subject, None);
        assert_eq!(result.updated[0].body_text, None);

        assert_eq!(result.deleted_server_ids, vec!["con:3".to_string()]);
    }
}
