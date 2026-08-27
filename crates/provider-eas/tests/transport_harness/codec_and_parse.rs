// SPDX-License-Identifier: MPL-2.0
//! Codec + status-table + rich-parse scenarios: every `WbxmlError` Display
//! arm (these strings reach users through `EasError::Wbxml`), the
//! [MS-ASCMD] §2.2.3.177.16 common-status table's totality, folder-type →
//! class mapping, and FolderSync change variants (Update / both Delete
//! forms / typed folders) through the real transport.

use std::{collections::HashSet, sync::Arc};

use provider_eas::{
    commands::{
        FH_ADD, FH_CHANGES, FH_DELETE, FH_DISPLAY_NAME, FH_FOLDER_SYNC, FH_PARENT_ID, FH_SERVER_ID,
        FH_STATUS, FH_SYNC_KEY, FH_TYPE, FH_UPDATE, PAGE_FOLDER, common_status_message,
        folder_sync_status_message, folder_type_to_class,
    },
    wbxml::WbxmlError,
};

use super::{
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// Every `WbxmlError` variant renders a distinguishing message — these
/// strings surface verbatim through `EasError::Wbxml` in user-facing errors.
#[test]
fn wbxml_error_display_names_every_variant() {
    let cases: Vec<(WbxmlError, &str)> = vec![
        (WbxmlError::UnexpectedEof, "unexpected end"),
        (WbxmlError::EmptyStream, "empty"),
        (WbxmlError::StringTableUnsupported, "string table"),
        (WbxmlError::UnknownCodePage(31), "code page 31"),
        (
            WbxmlError::UnsupportedGlobalToken(0x22),
            "global token 0x22",
        ),
        (WbxmlError::AttributesUnsupported(0x43), "attributes"),
        (
            WbxmlError::UnexpectedToken {
                expected: "TEXT",
                got: 0x05,
            },
            "expected text",
        ),
        (
            WbxmlError::UnexpectedEndOfDocument,
            "end of wbxml document unexpectedly",
        ),
        (WbxmlError::InvalidMultibyteInteger, "multibyte"),
        (WbxmlError::UnbalancedEnd, "unbalanced"),
        (WbxmlError::UnclosedTags, "unclosed"),
        (WbxmlError::NegativeOpaqueLength, "negative opaque"),
        (WbxmlError::InvalidUtf8, "utf-8"),
        (
            WbxmlError::InvalidContent("detail".into()),
            "invalid wbxml content: detail",
        ),
        (
            WbxmlError::UnexpectedTag {
                expected_page: 7,
                expected_token: 0x16,
                actual_page: 0,
                actual_token: 0x05,
            },
            "expected page 7",
        ),
    ];
    for (error, needle) in cases {
        let rendered = error.to_string();
        assert!(
            rendered
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase()),
            "Display for {error:?} must mention {needle:?}, got {rendered:?}"
        );
    }
    // The codec error also satisfies std::error::Error (usable in anyhow-like
    // chains by the host).
    let _: &dyn std::error::Error = &WbxmlError::UnexpectedEof;
}

/// The [MS-ASCMD] §2.2.3.177.16 common-status table: every code in its
/// documented range maps to a NON-EMPTY, NON-DUPLICATE message, and the
/// three genuinely-absent codes read as unknown (None) — a status the
/// table silently dropped would surface to users as "unknown status code".
#[test]
fn common_status_table_is_total_distinct_and_gapped_only_where_documented() {
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut mapped = 0;
    for code in 101..=177u32 {
        match common_status_message(code) {
            Some(message) => {
                assert!(!message.is_empty(), "code {code} maps to an empty message");
                assert!(
                    seen.insert(message),
                    "code {code} duplicates an earlier message: {message}"
                );
                mapped += 1;
            }
            None => assert!(
                (157..=159).contains(&code),
                "code {code} is unmapped outside the documented 157-159 gap"
            ),
        }
    }
    assert!(
        mapped > 60,
        "the table must carry its full documented range, saw {mapped} entries"
    );
    assert_eq!(
        common_status_message(999),
        None,
        "far-out codes are unknown"
    );
}

/// FolderSync's own status additions layer over the common table; the
/// unmapped codes fall through to it (2 → None via the common table).
#[test]
fn folder_sync_status_message_layers_over_the_common_table() {
    assert_eq!(folder_sync_status_message(1), "success");
    assert_eq!(folder_sync_status_message(3), "invalid synchronization key");
    assert_eq!(
        folder_sync_status_message(9),
        "folder hierarchy out of date"
    );
    assert_eq!(
        folder_sync_status_message(108),
        "device ID missing or invalid format"
    );
    assert_eq!(folder_sync_status_message(2), "unknown status code");
}

/// The EAS folder-type number → class mapping ([MS-ASFolderSync] §2.2.3).
#[test]
fn folder_type_maps_to_item_classes() {
    assert_eq!(folder_type_to_class("2"), "Email"); // Inbox
    assert_eq!(folder_type_to_class("7"), "Tasks");
    assert_eq!(folder_type_to_class("8"), "Calendar");
    assert_eq!(folder_type_to_class("9"), "Contacts");
    assert_eq!(folder_type_to_class("10"), "Notes"); // journal
    assert_eq!(folder_type_to_class("11"), "Notes");
    assert_eq!(folder_type_to_class("99"), "Email"); // unknown → mail
}

/// A FolderSync response carrying every change form: an Update, a Delete
/// with the spec's ServerId-child form, a Delete in the permissive text
/// form, and an unknown element the parser must ignore — plus a typed
/// (Calendar) folder whose class maps through `folder_type_to_class`.
#[tokio::test]
async fn folder_sync_parses_update_delete_and_typed_changes() {
    super::harness::init_logger();
    let response = provider_eas::wbxml::WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![
            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, "rich-key"),
            provider_eas::wbxml::WbxmlElement::container(
                PAGE_FOLDER,
                FH_CHANGES,
                vec![
                    // Update of a calendar folder (Type 8 → class "Calendar").
                    provider_eas::wbxml::WbxmlElement::container(
                        PAGE_FOLDER,
                        FH_UPDATE,
                        vec![
                            provider_eas::wbxml::WbxmlElement::text(
                                PAGE_FOLDER,
                                FH_SERVER_ID,
                                "fid-cal",
                            ),
                            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_PARENT_ID, "0"),
                            provider_eas::wbxml::WbxmlElement::text(
                                PAGE_FOLDER,
                                FH_DISPLAY_NAME,
                                "Calendar",
                            ),
                            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_TYPE, "8"),
                        ],
                    ),
                    // Spec-form Delete: ServerId child.
                    provider_eas::wbxml::WbxmlElement::container(
                        PAGE_FOLDER,
                        FH_DELETE,
                        vec![provider_eas::wbxml::WbxmlElement::text(
                            PAGE_FOLDER,
                            FH_SERVER_ID,
                            "fid-gone-1",
                        )],
                    ),
                    // Permissive-form Delete: text value directly.
                    provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_DELETE, "fid-gone-2"),
                    // Unknown element (e.g. Count metadata): ignored.
                    provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, 0x0D, "3"),
                ],
            ),
        ],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let result = client.folder_sync("0").await.expect("rich changes parse");
    assert_eq!(result.status, 1);
    assert_eq!(result.sync_key, "rich-key");
    assert_eq!(
        result.changes.len(),
        1,
        "Update lands in changes: {:?}",
        result.changes
    );
    assert_eq!(result.changes[0].server_id, "fid-cal");
    assert_eq!(result.changes[0].class, "Calendar");
    assert_eq!(result.changes[0].folder_type, Some(8));
    assert_eq!(
        result.deletions,
        vec!["fid-gone-1".to_owned(), "fid-gone-2".to_owned()],
        "both Delete forms parse"
    );
}

/// A FolderSync response whose Changes carry a malformed Add (missing the
/// required fields entirely is fine — defaults apply — but a parse must
/// still round-trip; here the focus is that an EMPTY change list with only
/// unknown tokens is a clean success).
#[tokio::test]
async fn folder_sync_ignores_unknown_change_elements() {
    super::harness::init_logger();
    let response = provider_eas::wbxml::WbxmlElement::container(
        PAGE_FOLDER,
        FH_FOLDER_SYNC,
        vec![
            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_STATUS, "1"),
            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_SYNC_KEY, "k2"),
            provider_eas::wbxml::WbxmlElement::container(
                PAGE_FOLDER,
                FH_CHANGES,
                vec![
                    // Add with unknown children only, plus a mail-folder Type
                    // (12 = user-created mail folder → class "Email").
                    provider_eas::wbxml::WbxmlElement::container(
                        PAGE_FOLDER,
                        FH_ADD,
                        vec![
                            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, 0x0D, "ignored"),
                            provider_eas::wbxml::WbxmlElement::text(PAGE_FOLDER, FH_TYPE, "12"),
                        ],
                    ),
                ],
            ),
        ],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let result = client.folder_sync("0").await.expect("tolerant parse");
    assert_eq!(result.changes.len(), 1);
    assert_eq!(
        result.changes[0].class, "Email",
        "Type 12 maps to the mail class"
    );
    assert_eq!(result.changes[0].folder_type, Some(12));
}
