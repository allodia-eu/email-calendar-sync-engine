// SPDX-License-Identifier: MPL-2.0
use super::{
    PAGE_COMPOSE, SendMailRequest, SmartForwardRequest, SmartReplyRequest, WbxmlElement,
    WbxmlError, compose, text_value,
};

// ============================================================================
// SendMail / SmartForward / SmartReply
// ============================================================================

/// Build a SendMail request.
///
/// WBXML shape (page 21 / ComposeMail):
/// ```xml
/// <SendMail>
///   <ClientId>{req.client_id}</ClientId>            <!-- STR_I, REQUIRED (MS-ASCMD 2.2.3.28.1) -->
///   <SaveInSentItems/>                              <!-- empty, optional -->
///   <Mime>{req.mime as raw RFC5322 bytes}</Mime>    <!-- OPAQUE (0xC3), NOT base64 -->
/// </SendMail>
/// ```
///
/// The `<Mime>` element MUST be emitted as WBXML OPAQUE (`0xC3` + mb_u_int32
/// length + raw bytes) per [MS-ASCMD] 2.2.3.11 — a STR_I `<Mime>` (inline
/// string) is misinterpreted by Exchange as a truncated text body and silently
/// corrupts the message. Token order (ClientId, SaveInSentItems, Mime) matches
/// the AOSP/mailkit reference and what Exchange expects.
///
/// `<ClientId>` is REQUIRED per [MS-ASCMD] 2.2.3.28.1 (Exchange 15.2 rejects
/// ClientId-less compose with in-body Status 103). A caller passing `None`
/// gets a synthesized id via [`crate::types::new_send_client_id`] so the
/// builder can never emit a spec-invalid request (production already
/// synthesizes upstream — this is the belt-and-braces guarantee).
pub fn build_send_mail_request(req: &SendMailRequest) -> WbxmlElement {
    let mut children = Vec::with_capacity(3);
    let synthesized;
    let cid = if let Some(c) = &req.client_id {
        c.as_str()
    } else {
        synthesized = crate::types::new_send_client_id("SM");
        synthesized.as_str()
    };
    children.push(WbxmlElement::text(PAGE_COMPOSE, compose::CLIENT_ID, cid));
    if req.save_to_sent {
        children.push(WbxmlElement::empty(
            PAGE_COMPOSE,
            compose::SAVE_IN_SENT_ITEMS,
        ));
    }
    children.push(WbxmlElement::opaque(
        PAGE_COMPOSE,
        compose::MIME,
        req.mime.clone(),
    ));
    WbxmlElement::container(PAGE_COMPOSE, compose::SEND_MAIL, children)
}

/// Shared child sequence for SmartForward / SmartReply.
///
/// Element order per the [MS-ASCMD] 6.41/6.43 request schemas — ClientId,
/// Source, SaveInSentItems, ReplaceMime, Mime. Exchange 15.2 enforces the
/// documented order: the pre-fix builders emitted SaveInSentItems + Mime
/// BEFORE Source with no ClientId and were rejected with in-body Status 103
/// ("invalid XML"), while the spec shape returned HTTP 200 empty body =
/// success (F10-3 live evidence 2026-08-02; Android `EasOutboxSync.writeTo`
/// corroborates ClientId-first, Source-before-Mime).
///
/// The `<Mime>` payload is the RAW RFC 5322 entity as WBXML OPAQUE. The
/// request DTOs carry base64 over IPC (`mime_base64`), so it is decoded here
/// — an invalid base64 payload is a client-side encoding error surfaced
/// BEFORE any bytes hit the wire. `SaveInSentItems` / `ReplaceMime` are
/// emitted only when the caller asked for them (the pre-fix builders emitted
/// `SaveInSentItems` unconditionally).
fn build_smart_send_children(
    client_id: Option<&str>,
    source_collection_id: &str,
    source_server_id: &str,
    save_to_sent: bool,
    replace_mime: bool,
    mime_base64: &str,
) -> Result<Vec<WbxmlElement>, WbxmlError> {
    use base64::Engine;
    let raw_mime = base64::engine::general_purpose::STANDARD
        .decode(mime_base64)
        .map_err(|e| WbxmlError::InvalidContent(format!("mime_base64 decode failed: {e}")))?;

    let mut children = Vec::with_capacity(5);
    // `<ClientId>` is REQUIRED per [MS-ASCMD] 2.2.3.28.1 (Exchange 15.2
    // rejects ClientId-less compose with Status 103 — see the module notes).
    // Synthesize when the caller passes None so the wire shape is always
    // spec-valid.
    let synthesized;
    let cid = if let Some(c) = client_id {
        c
    } else {
        synthesized = crate::types::new_send_client_id("SMRT-");
        synthesized.as_str()
    };
    children.push(WbxmlElement::text(PAGE_COMPOSE, compose::CLIENT_ID, cid));
    children.push(WbxmlElement::container(
        PAGE_COMPOSE,
        compose::SOURCE,
        vec![
            WbxmlElement::text(PAGE_COMPOSE, compose::FOLDER_ID, source_collection_id),
            WbxmlElement::text(PAGE_COMPOSE, compose::ITEM_ID, source_server_id),
        ],
    ));
    if save_to_sent {
        children.push(WbxmlElement::empty(
            PAGE_COMPOSE,
            compose::SAVE_IN_SENT_ITEMS,
        ));
    }
    if replace_mime {
        children.push(WbxmlElement::empty(PAGE_COMPOSE, compose::REPLACE_MIME));
    }
    children.push(WbxmlElement::opaque(PAGE_COMPOSE, compose::MIME, raw_mime));
    Ok(children)
}

/// Build a SmartForward request.
///
/// WBXML shape ([MS-ASCMD] 6.41 schema-documented order — ClientId, Source,
/// SaveInSentItems, ReplaceMime, Mime):
/// ```xml
/// <SmartForward>                        <!-- page 21, 0x06 -->
///   <ClientId>{cid}</ClientId>          <!-- 0x11, only when set -->
///   <Source>                            <!-- 0x0B -->
///     <FolderId>{collection}</FolderId> <!-- 0x0C -->
///     <ItemId>{server_id}</ItemId>      <!-- 0x0D -->
///   </Source>
///   <SaveInSentItems/>                  <!-- 0x08, only when save_to_sent -->
///   <ReplaceMime/>                      <!-- 0x09, only when replace_mime -->
///   <Mime>{raw RFC5322 bytes}</Mime>    <!-- 0x10 OPAQUE — raw, NOT base64 -->
/// </SmartForward>
/// ```
///
/// Decodes `req.mime_base64` to the raw MIME entity (see
/// `build_smart_send_children`); a decode failure is returned as
/// `WbxmlError::InvalidContent` before anything reaches the wire.
///
/// # Errors
///
/// Returns `WbxmlError::InvalidContent` when `mime_base64` does not decode —
/// nothing reaches the wire in that case.
pub fn build_smart_forward_request(req: &SmartForwardRequest) -> Result<WbxmlElement, WbxmlError> {
    Ok(WbxmlElement::container(
        PAGE_COMPOSE,
        compose::SMART_FORWARD,
        build_smart_send_children(
            req.client_id.as_deref(),
            &req.source_collection_id,
            &req.source_server_id,
            req.save_to_sent,
            req.replace_mime,
            &req.mime_base64,
        )?,
    ))
}

/// Build a SmartReply request. Same wire shape as SmartForward
/// ([MS-ASCMD] 6.43) under the SmartReply root (page 21, 0x07).
///
/// # Errors
///
/// Returns `WbxmlError::InvalidContent` when `mime_base64` does not decode —
/// nothing reaches the wire in that case.
pub fn build_smart_reply_request(req: &SmartReplyRequest) -> Result<WbxmlElement, WbxmlError> {
    Ok(WbxmlElement::container(
        PAGE_COMPOSE,
        compose::SMART_REPLY,
        build_smart_send_children(
            req.client_id.as_deref(),
            &req.source_collection_id,
            &req.source_server_id,
            req.save_to_sent,
            req.replace_mime,
            &req.mime_base64,
        )?,
    ))
}

/// Parse a SendMail/SmartForward/SmartReply response. They share the same
/// structure: an optional `<Status>` (token 0x12) child. An absent or empty
/// body is treated as success (status 1) per [MS-ASCMD] 2.2.3.162.6.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_send_mail_response(root: &WbxmlElement) -> Result<u32, WbxmlError> {
    for child in &root.children {
        if child.page == PAGE_COMPOSE && child.token == compose::STATUS {
            let status_str = text_value(child)?;
            return status_str.parse::<u32>().map_err(|_| {
                WbxmlError::InvalidContent(format!("non-numeric status: {status_str}"))
            });
        }
    }
    Ok(1) // success default
}
