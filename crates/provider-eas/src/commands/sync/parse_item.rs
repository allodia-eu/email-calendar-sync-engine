// SPDX-License-Identifier: MPL-2.0
// Per-item Sync command parsing (Email / Calendar / Contacts).

use crate::{
    calendar::{CalendarEventProps, parse_location_16x},
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_CHANGE, AS_DELETE, AS_SERVER_ID, CalendarItemWithId,
        ContactsItemWithId, EasAttachment, EasItem, MeetingRequestInfo, PAGE_AIRSYNC, WbxmlElement,
        WbxmlError, WbxmlValue, text_value, text_value_opt,
    },
    contacts::ContactsContactProps,
    wbxml::tags::{base, pages},
};
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
            (PAGE_AIRSYNC, AS_ADD | AS_CHANGE) => {
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
pub(super) fn parse_sync_commands(
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
pub(super) fn parse_calendar_sync_commands(
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
pub(super) fn parse_contacts_sync_commands(
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
                item.meeting_message_type = raw.as_deref().and_then(|s| {
                    if let Ok(n) = s.parse() {
                        Some(n)
                    } else {
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
            // Tags we deliberately ignore for MVP — metadata we don't model
            // yet or already consumed at a higher level (e.g. Status on
            // ApplicationData belongs to the Sync command, not the item) —
            // plus unknown tags: ignore.
            _ => {}
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
                    info.location = parse_location_16x("email MeetingRequest", child);
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
                info.instance_type = raw.as_deref().and_then(|s| {
                    if let Ok(n) = s.parse() {
                        Some(n)
                    } else {
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
            // EstimatedDataSize (not surfaced on EasItem) and unknown tags:
            // ignore.
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
            item.body_html.clone_from(&data);
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
