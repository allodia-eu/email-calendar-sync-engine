// SPDX-License-Identifier: MPL-2.0
use super::*;

// ============================================================================
// ItemOperations (fetch attachments / items)
// ============================================================================

/// Build an ItemOperations Fetch request.
///
/// Wire shape per [MS-ASCMD] §6.23 (request schema) and [MS-ASWBXML]
/// §2.1.2.1.21 (page 20):
/// ```xml
/// <ItemOperations>                                   <!-- page 20, 0x05 -->
///   <Fetch>                                          <!-- page 20, 0x06 -->
///     <Store>Mailbox</Store>                         <!-- page 20, 0x07 -->
///     <airsyncbase:FileReference>{ref}</>            <!-- page 17, 0x11 (attachment) -->
///     — OR —
///     <airsync:CollectionId>{collection_id}</>       <!-- page 0, 0x12 (item fetch) -->
///     <airsync:ServerId>{server_id}</>               <!-- page 0, 0x0D -->
///     <Options>                                      <!-- page 20, 0x08 -->
///       <airsyncbase:BodyPreference>                 <!-- page 17, 0x05 -->
///         <airsyncbase:Type>2</>                     <!-- page 17, 0x06 (HTML) -->
///       </>
///     </Options>
///   </Fetch>
/// </ItemOperations>
/// ```
/// CollectionId/ServerId/FileReference are NOT page-20 tokens — the request
/// schema (§6.23) imports them from the AirSync / AirSyncBase namespaces.
/// The BodyPreference on item fetches is required in practice: without it
/// Exchange 2019 answers with the airsyncbase:Body *metadata* only (Type +
/// EstimatedDataSize + Truncated=1, no Data child) — live evidence:
/// eas_io_debug raw dump, 2026-08-02.
///
/// Three fetch forms, in precedence order (first populated wins):
/// 1. **Attachment fetch** (`file_reference`): Store + airsyncbase:FileReference.
/// 2. **Search-result fetch** (`long_id`, [MS-ASCMD] §4.10.3.3): Store + search:LongId (page 15,
///    0x18) + Options>BodyPreference(Type 2). The search:LongId replaces CollectionId/ServerId
///    (§2.2.3.98.1: they MUST NOT accompany it — that rule is stated for MeetingResponse/Source but
///    the Fetch schema is a `xs:choice` with the same exclusivity).
/// 3. **Body/item fetch** (`collection_id` + `server_id`): Store + airsync:CollectionId +
///    airsync:ServerId + Options>BodyPreference(Type 2). With `mime: true` the Options carry
///    `airsync:MIMESupport`=2 ahead of the BodyPreference and its Type switches to 4 — the
///    MIME-fetch shape of [MS-ASCMD] §4.10.2.1.
pub fn build_item_operations_request(req: &ItemOperationsFetchRequest) -> WbxmlElement {
    use tags::item_operations as io;

    let mut fetch_children = vec![WbxmlElement::text(
        PAGE_ITEM_OPS,
        io::STORE,
        "Mailbox".to_string(),
    )];

    if let Some(file_ref) = &req.file_reference {
        fetch_children.push(WbxmlElement::text(
            pages::BASE,
            tags::base::FILE_REFERENCE,
            file_ref.clone(),
        ));
    } else if let Some(long_id) = &req.long_id {
        fetch_children.push(WbxmlElement::text(
            tags::search::PAGE,
            tags::search::LONG_ID,
            long_id.clone(),
        ));
        // Same HTML-body requirement as the collection/server-id form below.
        let body_preference = WbxmlElement::container(
            pages::BASE,
            tags::base::BODY_PREFERENCE,
            vec![WbxmlElement::text(pages::BASE, tags::base::TYPE, "2")],
        );
        fetch_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            vec![body_preference],
        ));
    } else {
        fetch_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_COLLECTION_ID,
            req.collection_id.clone(),
        ));
        fetch_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_SERVER_ID,
            req.server_id.clone(),
        ));
        // Item fetch: ask for the body explicitly. Without a BodyPreference
        // the server returns Body metadata (Type / EstimatedDataSize /
        // Truncated) but no Data child.
        //
        // Options children per [MS-ASCMD] §4.10.2.1 (MIME fetch example):
        // `airsync:MIMESupport` (page 0, 0x22) BEFORE
        // `airsyncbase:BodyPreference`. MIME fetches switch the BodyPreference
        // Type to 4 (MIME BLOB); the default stays Type 2 (HTML).
        let mut options_children: Vec<WbxmlElement> = Vec::new();
        let body_type = if req.mime {
            // Level 2 = "send MIME data for all messages" (§2.2.3.110.3).
            // The spec example uses 1 (S/MIME messages only), but a MIME
            // fetch here means the caller wants the raw message for THIS
            // item regardless of its S/MIME status — level 2 is the
            // conservative superset that never withholds the MIME BLOB.
            options_children.push(WbxmlElement::text(
                PAGE_AIRSYNC,
                AS_MIME_SUPPORT,
                "2".to_string(),
            ));
            "4"
        } else {
            "2"
        };
        options_children.push(WbxmlElement::container(
            pages::BASE,
            tags::base::BODY_PREFERENCE,
            vec![WbxmlElement::text(pages::BASE, tags::base::TYPE, body_type)],
        ));
        fetch_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            options_children,
        ));
    }

    WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::FETCH,
            fetch_children,
        )],
    )
}

/// Parse an ItemOperations Fetch response.
///
/// Wire shape per [MS-ASCMD] §6.24 (response schema):
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Status>1</Status>                  <!-- page 20, 0x0D — command level -->
///   <Response>                          <!-- page 20, 0x0E -->
///     <Fetch>                           <!-- page 20, 0x06 -->
///       <Status>1</Status>              <!-- page 20, 0x0D — per-op level -->
///       <Properties>                    <!-- page 20, 0x0B -->
///         <Data>{base64}</Data>         <!-- page 20, 0x0C -->
///         <airsyncbase:ContentType>../> <!-- page 17, 0x17 (§2.2.3.139.2) -->
///       </Properties>
///     </Fetch>
///   </Response>
/// </ItemOperations>
/// ```
/// The fetch-level Status overrides the top-level one (more specific wins),
/// mirroring the Sync parser's collection-status rule.
pub fn parse_item_operations_response(
    root: &WbxmlElement,
) -> Result<ItemOperationsFetchResult, WbxmlError> {
    use tags::item_operations as io;

    let mut result = ItemOperationsFetchResult::default();
    // Top-level Status first (command-level rejection, e.g. 143 device not
    // provisioned); a fetch-level Status below overrides it.
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS
            && child.token == io::STATUS
            && let Ok(s) = text_value(child)
            && let Ok(n) = s.parse::<u8>()
        {
            result.status = n;
        }
    }
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_ITEM_OPS && resp_child.token == io::FETCH {
                    for fetch_child in &resp_child.children {
                        match (fetch_child.page, fetch_child.token) {
                            (PAGE_ITEM_OPS, io::STATUS) => {
                                if let Ok(s) = text_value(fetch_child)
                                    && let Ok(n) = s.parse::<u8>()
                                {
                                    result.status = n;
                                }
                            }
                            (PAGE_ITEM_OPS, io::PROPERTIES) => {
                                for prop in &fetch_child.children {
                                    match (prop.page, prop.token) {
                                        (PAGE_ITEM_OPS, io::DATA) => {
                                            result.data = match &prop.value {
                                                WbxmlValue::Text(t) => Some(t.clone()),
                                                WbxmlValue::Opaque(b) => Some(base64_encode(b)),
                                                WbxmlValue::Empty => None,
                                            };
                                        }
                                        // airsyncbase:ContentType (page 17, 0x17)
                                        (pages::BASE, tags::base::CONTENT_TYPE) => {
                                            result.content_type = match &prop.value {
                                                WbxmlValue::Text(t) => Some(t.clone()),
                                                _ => None,
                                            };
                                        }
                                        // airsyncbase:Body (page 17, 0x0A) —
                                        // the payload of an item/body fetch.
                                        // Its Data child (page 17, 0x0B)
                                        // carries the body text; Type tells us
                                        // whether it is HTML (2) or plain (1),
                                        // which we surface as content_type
                                        // when the server didn't send one.
                                        (pages::BASE, tags::base::BODY) => {
                                            let mut body_type: Option<u8> = None;
                                            for b in &prop.children {
                                                match (b.page, b.token) {
                                                    (pages::BASE, tags::base::TYPE) => {
                                                        body_type = text_value_opt(b)
                                                            .and_then(|s| s.parse().ok());
                                                    }
                                                    (pages::BASE, tags::base::DATA) => {
                                                        result.data = match &b.value {
                                                            WbxmlValue::Text(t) => Some(t.clone()),
                                                            WbxmlValue::Opaque(bytes) => {
                                                                Some(base64_encode(bytes))
                                                            }
                                                            WbxmlValue::Empty => None,
                                                        };
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            if result.content_type.is_none() {
                                                result.content_type = match body_type {
                                                    Some(2) => Some("text/html".to_string()),
                                                    Some(1) => Some("text/plain".to_string()),
                                                    // Type 4 = MIME BLOB
                                                    // ([MS-ASCMD] §2.2.3.110.3):
                                                    // the payload is a raw
                                                    // RFC 5322 message.
                                                    Some(4) => Some("message/rfc822".to_string()),
                                                    _ => None,
                                                };
                                            }
                                        }
                                        _ => {}
                                    }
                                }
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
// ItemOperations → EmptyFolderContents
// ============================================================================

/// Build an ItemOperations → EmptyFolderContents request ([MS-ASCMD]
/// §4.14.4.1; ItemOperations code page 20 per [MS-ASWBXML] §2.1.2.1.21).
/// This is a SEPARATE builder from `build_item_operations_request` — the
/// fetch builder's shape is Fetch-specific and must not be overloaded.
///
/// Wire shape:
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <EmptyFolderContents>               <!-- page 20, 0x12 -->
///     <airsync:CollectionId>15</>       <!-- page 0, 0x12 — AirSync page -->
///     [<Options>                        <!-- page 20, 0x08 -->
///       <DeleteSubFolders/>             <!-- page 20, 0x13 — empty element -->
///     </Options>]
///   </EmptyFolderContents>
/// </ItemOperations>
/// ```
/// The Options element (and its DeleteSubFolders child) is emitted ONLY
/// when `req.delete_sub_folders` is true — the server default keeps
/// subfolders, and the §4.14.4.1 example carries no Options at all.
///
/// DESTRUCTIVE: this deletes every item in the folder server-side (and,
/// with `delete_sub_folders`, the subfolders too). Callers must confirm
/// with the user before invoking.
pub fn build_empty_folder_contents_request(req: &EmptyFolderContentsRequest) -> WbxmlElement {
    use tags::item_operations as io;

    let mut efc_children = vec![WbxmlElement::text(
        PAGE_AIRSYNC,
        AS_COLLECTION_ID,
        req.collection_id.clone(),
    )];
    if req.delete_sub_folders {
        efc_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            vec![WbxmlElement::empty(PAGE_ITEM_OPS, io::DELETE_SUB_FOLDERS)],
        ));
    }

    WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::EMPTY_FOLDER_CONTENTS,
            efc_children,
        )],
    )
}

/// Parse an ItemOperations → EmptyFolderContents response ([MS-ASCMD]
/// §4.14.4.2):
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Status>1</Status>                  <!-- page 20, 0x0D — command level -->
///   <Response>                          <!-- page 20, 0x0E -->
///     <EmptyFolderContents>             <!-- page 20, 0x12 -->
///       <Status>1</Status>              <!-- page 20, 0x0D — element level -->
///       <airsync:CollectionId>15</>     <!-- page 0, 0x12 — echo -->
///     </EmptyFolderContents>
///   </Response>
/// </ItemOperations>
/// ```
/// Nested-Status rule mirrors the ItemOperations fetch parser and the
/// Settings family: the top-level Status is read first (command-level
/// rejection, e.g. 143 device not provisioned — then no Response element
/// is present at all), and the EmptyFolderContents-level Status overrides
/// it when present (more specific wins). Both stay surfaced: the specific
/// one on `empty_status`. A missing Status defaults to 1 (success),
/// mirroring GetItemEstimate. Malformed Status values are warn-logged and
/// defaulted — never swallowed.
pub fn parse_empty_folder_contents_response(
    root: &WbxmlElement,
) -> Result<EmptyFolderContentsResult, WbxmlError> {
    use tags::item_operations as io;

    expect_tag(root, PAGE_ITEM_OPS, io::ITEM_OPERATIONS)?;
    let mut result = EmptyFolderContentsResult {
        status: 1, // success default when Status elements are absent
        ..EmptyFolderContentsResult::default()
    };
    // Top-level Status first; an EmptyFolderContents-level Status below
    // overrides it (same ordering as parse_item_operations_response).
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = match raw.parse() {
                Ok(n) => n,
                Err(_) => {
                    log::warn!(
                        "ItemOperations EmptyFolderContents: malformed top-level Status \"{raw}\"; defaulting to 1"
                    );
                    1
                }
            };
        }
    }
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_ITEM_OPS && resp_child.token == io::EMPTY_FOLDER_CONTENTS
                {
                    for efc_child in &resp_child.children {
                        match (efc_child.page, efc_child.token) {
                            (PAGE_ITEM_OPS, io::STATUS) => {
                                let raw = text_value(efc_child).unwrap_or_default();
                                let n: u32 = match raw.parse() {
                                    Ok(n) => n,
                                    Err(_) => {
                                        log::warn!(
                                            "ItemOperations EmptyFolderContents: malformed EmptyFolderContents Status \"{raw}\"; defaulting to 1"
                                        );
                                        1
                                    }
                                };
                                result.empty_status = Some(n);
                                result.status = n; // more specific wins
                            }
                            (PAGE_AIRSYNC, AS_COLLECTION_ID) => {
                                result.collection_id = match text_value(efc_child) {
                                    Ok(s) => Some(s),
                                    Err(e) => {
                                        // Undecodable CollectionId is malformed
                                        // server data; drop the echo but never
                                        // silently.
                                        log::warn!(
                                            "ItemOperations EmptyFolderContents: skipping undecodable CollectionId echo: {e}"
                                        );
                                        None
                                    }
                                };
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
// ItemOperations → Move (conversation move)
// ============================================================================

/// Build an ItemOperations → Move request ([MS-ASCMD] §4.25.1;
/// ItemOperations code page 20 per [MS-ASWBXML] §2.1.2.1.21). This is a
/// SEPARATE builder from `build_item_operations_request` — the fetch
/// builder's shape is Fetch-specific and must not be overloaded. Note the
/// Move token here is the ItemOperations-namespace (page 20) 0x16 — NOT
/// the MoveItems-page (5) Move token.
///
/// Wire shape:
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Move>                              <!-- page 20, 0x16 -->
///     <DstFldId>15</DstFldId>           <!-- page 20, 0x17 -->
///     <ConversationId>...</>            <!-- page 20, 0x18 — OPAQUE bytes -->
///     [<Options>                        <!-- page 20, 0x08 -->
///       <MoveAlways/>                   <!-- page 20, 0x19 — empty element -->
///     </Options>]
///   </Move>
/// </ItemOperations>
/// ```
/// ConversationId is serialized as OPAQUE binary, carried verbatim (never
/// base64-decoded or re-encoded) — the same convention as the email2
/// ConversationId parse path. The Options element (and its MoveAlways
/// child) is emitted ONLY when `req.move_always` is true.
///
/// MOVE_ALWAYS moves all FUTURE messages of the conversation to the
/// destination folder too — a persistent server-side behavior. Callers
/// must surface this to the user before setting it.
pub fn build_conversation_move_request(req: &ConversationMoveRequest) -> WbxmlElement {
    use tags::item_operations as io;

    let mut move_children = vec![
        WbxmlElement::text(PAGE_ITEM_OPS, io::DST_FLD_ID, req.dst_folder_id.clone()),
        WbxmlElement::opaque(
            PAGE_ITEM_OPS,
            io::CONVERSATION_ID,
            req.conversation_id.clone(),
        ),
    ];
    if req.move_always {
        move_children.push(WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::OPTIONS,
            vec![WbxmlElement::empty(PAGE_ITEM_OPS, io::MOVE_ALWAYS)],
        ));
    }

    WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::MOVE,
            move_children,
        )],
    )
}

/// Parse an ItemOperations → Move response ([MS-ASCMD] §4.25.2):
/// ```xml
/// <ItemOperations>                      <!-- page 20, 0x05 -->
///   <Status>1</Status>                  <!-- page 20, 0x0D — command level -->
///   <Response>                          <!-- page 20, 0x0E -->
///     <Move>                            <!-- page 20, 0x16 -->
///       <Status>1</Status>              <!-- page 20, 0x0D — element level -->
///       <ConversationId>...</>          <!-- page 20, 0x18 — echo -->
///     </Move>
///   </Response>
/// </ItemOperations>
/// ```
/// Nested-Status rule mirrors the ItemOperations fetch parser and the
/// Settings family: the top-level Status is read first (command-level
/// rejection, e.g. 143 device not provisioned — then no Response element
/// is present at all), and the Move-level Status overrides it when present
/// (more specific wins). Both stay surfaced: the specific one on
/// `move_status`. A missing Status defaults to 1 (success), mirroring
/// GetItemEstimate. Malformed Status values are warn-logged and defaulted
/// — never swallowed.
///
/// The ConversationId echo is opaque binary on the wire, but some
/// deployments serialize it as base64 *text* — handle both and keep the
/// bytes verbatim (never decoded), the same convention as the email2
/// ConversationId path. A missing or empty echo maps to `None` (not
/// `Some(vec![])`), since empty != absent.
pub fn parse_conversation_move_response(
    root: &WbxmlElement,
) -> Result<ConversationMoveResult, WbxmlError> {
    use tags::item_operations as io;

    expect_tag(root, PAGE_ITEM_OPS, io::ITEM_OPERATIONS)?;
    let mut result = ConversationMoveResult {
        status: 1, // success default when Status elements are absent
        ..ConversationMoveResult::default()
    };
    // Top-level Status first; a Move-level Status below overrides it (same
    // ordering as parse_item_operations_response).
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = match raw.parse() {
                Ok(n) => n,
                Err(_) => {
                    log::warn!(
                        "ItemOperations Move: malformed top-level Status \"{raw}\"; defaulting to 1"
                    );
                    1
                }
            };
        }
    }
    for child in &root.children {
        if child.page == PAGE_ITEM_OPS && child.token == io::RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_ITEM_OPS && resp_child.token == io::MOVE {
                    for move_child in &resp_child.children {
                        match (move_child.page, move_child.token) {
                            (PAGE_ITEM_OPS, io::STATUS) => {
                                let raw = text_value(move_child).unwrap_or_default();
                                let n: u32 = match raw.parse() {
                                    Ok(n) => n,
                                    Err(_) => {
                                        log::warn!(
                                            "ItemOperations Move: malformed Move Status \"{raw}\"; defaulting to 1"
                                        );
                                        1
                                    }
                                };
                                result.move_status = Some(n);
                                result.status = n; // more specific wins
                            }
                            (PAGE_ITEM_OPS, io::CONVERSATION_ID) => {
                                result.conversation_id = match &move_child.value {
                                    WbxmlValue::Opaque(b) if !b.is_empty() => Some(b.clone()),
                                    WbxmlValue::Text(s) if !s.is_empty() => {
                                        Some(s.as_bytes().to_vec())
                                    }
                                    _ => None,
                                };
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
