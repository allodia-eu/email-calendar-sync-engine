// SPDX-License-Identifier: MPL-2.0
//! Canned WBXML response trees for the mock server, built with the crate's
//! OWN serializer and public tag constants (self-consistent with the codec
//! under test — the `provider-graph` fixtures' convention). Each builder is
//! named for the command response it shapes; scenario files combine them
//! with `MockResponse` headers.

use provider_eas::{
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_CHANGE, AS_CLIENT_ID, AS_COLLECTION, AS_COLLECTIONS,
        AS_COMMANDS, AS_DELETE, AS_MORE_AVAILABLE, AS_RESPONSES, AS_SERVER_ID, AS_STATUS, AS_SYNC,
        AS_SYNC_KEY, FH_ADD, FH_CHANGES, FH_DISPLAY_NAME, FH_FOLDER_CREATE, FH_FOLDER_DELETE,
        FH_FOLDER_SYNC, FH_FOLDER_UPDATE, FH_PARENT_ID, FH_SERVER_ID, FH_STATUS, FH_SYNC_KEY,
        FH_TYPE, PAGE_AIRSYNC, PAGE_FOLDER, PAGE_ITEM_OPS,
    },
    wbxml::{
        WbxmlElement,
        tags::{base, email, item_operations, pages, provision, settings},
    },
};

// ---- AirSync (page 0): Sync / sync-change responses ----

/// A full Sync response: one collection, key rotation, optional
/// `MoreAvailable`, one Add per `(server_id, subject)` pair.
pub(crate) fn sync_response(
    collection_status: &str,
    new_key: &str,
    more_available: bool,
    adds: &[(&str, &str)],
) -> WbxmlElement {
    let mut collection = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
        WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, collection_status),
    ];
    if more_available {
        collection.push(WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE));
    }
    if !adds.is_empty() {
        collection.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COMMANDS,
            adds.iter()
                .map(|&(id, subject)| {
                    WbxmlElement::container(
                        PAGE_AIRSYNC,
                        AS_ADD,
                        vec![
                            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, id),
                            email_app_data(subject),
                        ],
                    )
                })
                .collect(),
        ));
    }
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                collection,
            )],
        )],
    )
}

/// A minimal email `ApplicationData` (Subject + HTML body) — the same shape
/// the command tests use.
fn email_app_data(subject: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(email::PAGE, email::SUBJECT, subject),
            WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "2"),
                    WbxmlElement::text(pages::BASE, base::DATA, "<p>body</p>"),
                ],
            ),
        ],
    )
}

/// A Sync change-upsync response: rotated key + per-item `Responses`
/// statuses ([MS-ASSYNC] §2.2.2).
pub(crate) fn sync_change_response(new_key: &str, response_ids: &[(&str, &str)]) -> WbxmlElement {
    let responses = response_ids
        .iter()
        .map(|&(id, status)| {
            WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_CHANGE,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_CLIENT_ID, id),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, status),
                ],
            )
        })
        .collect();
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    WbxmlElement::container(PAGE_AIRSYNC, AS_RESPONSES, responses),
                ],
            )],
        )],
    )
}

/// A Sync response carrying only a delete.
pub(crate) fn sync_delete_response(new_key: &str, deleted_id: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_COLLECTION,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, new_key),
                    WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
                    WbxmlElement::container(
                        PAGE_AIRSYNC,
                        AS_COMMANDS,
                        vec![WbxmlElement::container(
                            PAGE_AIRSYNC,
                            AS_DELETE,
                            vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, deleted_id)],
                        )],
                    ),
                ],
            )],
        )],
    )
}

// ---- FolderHierarchy (page 7) ----

/// A FolderSync response with adds (id, parent, name, type).
pub(crate) fn folder_sync_response(
    new_key: &str,
    adds: &[(&str, &str, &str, &str)],
) -> WbxmlElement {
    let changes: Vec<WbxmlElement> = adds
        .iter()
        .map(|&(id, parent, name, typ)| {
            WbxmlElement::container(
                PAGE_FOLDER,
                FH_ADD,
                vec![
                    WbxmlElement::text(PAGE_FOLDER, FH_SERVER_ID, id),
                    WbxmlElement::text(PAGE_FOLDER, FH_PARENT_ID, parent),
                    WbxmlElement::text(PAGE_FOLDER, FH_DISPLAY_NAME, name),
                    WbxmlElement::text(PAGE_FOLDER, FH_TYPE, typ),
                ],
            )
        })
        .collect();
    let mut children = vec![
        WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
        WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, new_key),
    ];
    if !changes.is_empty() {
        children.push(WbxmlElement::container(PAGE_FOLDER, FH_CHANGES, changes));
    }
    WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, children)
}

/// A FolderSync response carrying only a top-level Status (error or the
/// in-body 142-144 provision demand).
pub(crate) fn folder_sync_status(status: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![WbxmlElement::text(PAGE_FOLDER, FH_STATUS, status)],
    )
}

/// A folder-op response (FolderCreate/Delete/Update share the shape).
pub(crate) fn folder_op_response(root_token: u8, status: &str, new_key: &str) -> WbxmlElement {
    WbxmlElement::container(
        PAGE_FOLDER,
        root_token,
        vec![
            WbxmlElement::text(PAGE_FOLDER, FH_STATUS, status),
            WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, new_key),
        ],
    )
}

/// Page-7 root tokens for the folder-op responses.
pub(crate) const FOLDER_CREATE_ROOT: u8 = FH_FOLDER_CREATE;
/// Page-7 FolderDelete root token.
pub(crate) const FOLDER_DELETE_ROOT: u8 = FH_FOLDER_DELETE;
/// Page-7 FolderUpdate root token.
pub(crate) const FOLDER_UPDATE_ROOT: u8 = FH_FOLDER_UPDATE;

// ---- Provision (page 14) ----

/// A Provision response (both phases share the shape): status + policy key.
pub(crate) fn provision_response(status: &str, policy_key: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::PROVISION,
        provision::PROVISION,
        vec![
            WbxmlElement::text(pages::PROVISION, provision::STATUS, status),
            WbxmlElement::container(
                pages::PROVISION,
                provision::POLICIES,
                vec![WbxmlElement::container(
                    pages::PROVISION,
                    provision::POLICY,
                    vec![WbxmlElement::text(
                        pages::PROVISION,
                        provision::POLICY_KEY,
                        policy_key,
                    )],
                )],
            ),
        ],
    )
}

/// A Provision response carrying a `<RemoteWipe>` demand — per [MS-ASPROV]
/// §2.2.2.6 it is a DIRECT child of Provision (the parser matches it there).
pub(crate) fn provision_remote_wipe() -> WbxmlElement {
    WbxmlElement::container(
        pages::PROVISION,
        provision::PROVISION,
        vec![
            WbxmlElement::text(pages::PROVISION, provision::STATUS, "1"),
            WbxmlElement::empty(pages::PROVISION, provision::REMOTE_WIPE),
        ],
    )
}

// ---- Settings (page 18) ----

/// A Settings response: top status + an arbitrary second-level element
/// (UserInformation / Oof / DevicePassword) with its own status + payload.
pub(crate) fn settings_response(second_level: WbxmlElement) -> WbxmlElement {
    WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            second_level,
        ],
    )
}

/// UserInformation > Get > EmailAddresses > SMTP (one address).
pub(crate) fn user_information_element(smtp: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::SETTINGS,
        settings::USER_INFORMATION,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::GET,
                vec![WbxmlElement::container(
                    pages::SETTINGS,
                    0x1E,                                                  // EmailAddresses
                    vec![WbxmlElement::text(pages::SETTINGS, 0x1F, smtp)], // SMTP
                )],
            ),
        ],
    )
}

/// Oof > Get with OofState + one internal reply message.
pub(crate) fn oof_get_element(state: &str, reply: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::SETTINGS,
        settings::OOF,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::GET,
                vec![
                    WbxmlElement::text(pages::SETTINGS, settings::OOF_STATE, state),
                    WbxmlElement::container(
                        pages::SETTINGS,
                        settings::OOF_MESSAGE,
                        vec![
                            WbxmlElement::empty(pages::SETTINGS, settings::APPLIES_TO_INTERNAL),
                            WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "1"),
                            WbxmlElement::text(pages::SETTINGS, settings::REPLY_MESSAGE, reply),
                            WbxmlElement::text(pages::SETTINGS, settings::BODY_TYPE, "Text"),
                        ],
                    ),
                ],
            ),
        ],
    )
}

/// Oof with only a Set-level status (the Set-form response shape).
pub(crate) fn oof_set_element(status: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::SETTINGS,
        settings::OOF,
        vec![WbxmlElement::text(
            pages::SETTINGS,
            settings::STATUS,
            status,
        )],
    )
}

/// DevicePassword with a Set-level status.
pub(crate) fn device_password_element(status: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::SETTINGS,
        settings::DEVICE_PASSWORD,
        vec![WbxmlElement::text(
            pages::SETTINGS,
            settings::STATUS,
            status,
        )],
    )
}

/// DeviceInformation > Status (the DI ack shape `parse_settings_response`
/// reads; the top-level Settings status rides alongside).
pub(crate) fn device_information_element(di_status: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::SETTINGS,
        settings::DEVICE_INFORMATION,
        vec![WbxmlElement::text(
            pages::SETTINGS,
            settings::STATUS,
            di_status,
        )],
    )
}

// ---- ItemOperations (page 20) ----

/// An ItemOperations Fetch response with an inline Properties > Data body
/// (base64 text) — the non-multipart fetch shape.
pub(crate) fn fetch_response(data: &str, content_type: &str) -> WbxmlElement {
    fetch_tree(None, Some((data, content_type)))
}

/// An ItemOperations Fetch response whose body is an airsyncbase:Body with
/// Type + Data — the item/body fetch shape.
pub(crate) fn fetch_body_response(body_type: &str, data: &str) -> WbxmlElement {
    fetch_tree(Some((body_type, data)), None)
}

/// An ItemOperations Fetch response whose body is an airsyncbase:Body with
/// an `itemoperations:Part` index — the multipart form; `parts` must carry
/// the WBXML tree as part 0 (see [`multipart_tree`]).
pub(crate) fn fetch_part_response(part_index: &str) -> WbxmlElement {
    let body = WbxmlElement::container(
        pages::BASE,
        base::BODY,
        vec![WbxmlElement::text(
            PAGE_ITEM_OPS,
            item_operations::PART,
            part_index,
        )],
    );
    WbxmlElement::container(
        PAGE_ITEM_OPS,
        item_operations::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, item_operations::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                item_operations::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    item_operations::FETCH,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, item_operations::STATUS, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            item_operations::PROPERTIES,
                            vec![body],
                        ),
                    ],
                )],
            ),
        ],
    )
}

/// Wrap a fetch response tree as multipart part 0 plus one payload part —
/// `MockResponse::multipart(&multipart_tree(tree, payload))`.
pub(crate) fn multipart_tree(tree: &WbxmlElement, payload: &[u8]) -> Vec<Vec<u8>> {
    vec![
        provider_eas::wbxml::serialize_tree(tree).expect("part 0 serializes"),
        payload.to_vec(),
    ]
}

fn fetch_tree(body: Option<(&str, &str)>, inline: Option<(&str, &str)>) -> WbxmlElement {
    let mut properties = Vec::new();
    if let Some((data, content_type)) = inline {
        properties.push(WbxmlElement::text(
            PAGE_ITEM_OPS,
            item_operations::DATA,
            data,
        ));
        properties.push(WbxmlElement::text(
            pages::BASE,
            base::CONTENT_TYPE,
            content_type,
        ));
    }
    if let Some((body_type, data)) = body {
        properties.push(WbxmlElement::container(
            pages::BASE,
            base::BODY,
            vec![
                WbxmlElement::text(pages::BASE, base::TYPE, body_type),
                WbxmlElement::text(pages::BASE, base::DATA, data),
            ],
        ));
    }
    WbxmlElement::container(
        PAGE_ITEM_OPS,
        item_operations::ITEM_OPERATIONS,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, item_operations::STATUS, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                item_operations::RESPONSE,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    item_operations::FETCH,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, item_operations::STATUS, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            item_operations::PROPERTIES,
                            properties,
                        ),
                    ],
                )],
            ),
        ],
    )
}
