// SPDX-License-Identifier: MPL-2.0
//! Settings UserInformation (Get form, [MS-ASCMD] §4.21).

use super::*;

// ---- Settings UserInformation (Get form, [MS-ASCMD] §4.21) ----

/// Request shape per [MS-ASCMD] §4.21 and [MS-ASWBXML] §2.1.2.1.19
/// (Settings code page 18):
/// ```text
/// Settings (18,0x05) > UserInformation (18,0x1D) > Get (18,0x07)
/// ```
/// `Get` is an empty element — the Get form carries no children.
#[test]
fn settings_user_information_request_uses_spec_shape() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = build_settings_user_information_request();
    assert_eq!(
        (tree.page, tree.token),
        (pages::SETTINGS, settings::SETTINGS)
    );
    assert_eq!(tree.children.len(), 1);
    let ui = &tree.children[0];
    assert_eq!(
        (ui.page, ui.token),
        (pages::SETTINGS, settings::USER_INFORMATION)
    );
    assert_eq!(ui.children.len(), 1);
    let get = &ui.children[0];
    assert_eq!((get.page, get.token), (pages::SETTINGS, settings::GET));
    assert!(get.children.is_empty());
    assert!(matches!(get.value, WbxmlValue::Empty));
}

#[test]
fn settings_user_information_request_round_trips() {
    let tree = build_settings_user_information_request();
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Response shape per [MS-ASCMD] §4.21:
/// ```text
/// Settings (18,0x05) > Status (18,0x06), UserInformation (18,0x1D) >
///   Status (18,0x06), Get (18,0x07) > EmailAddresses (18,0x1E) >
///   SMTPAddress (18,0x1F) (1..n)
/// ```
/// Both SMTPAddress entries land in `email_addresses` in wire order, and
/// both Status levels are surfaced.
#[test]
fn settings_user_information_response_parses_email_addresses() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
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
                            settings::EMAIL_ADDRESSES,
                            vec![
                                WbxmlElement::text(
                                    pages::SETTINGS,
                                    settings::SMTP_ADDRESS,
                                    "alice@example.com",
                                ),
                                WbxmlElement::text(
                                    pages::SETTINGS,
                                    settings::SMTP_ADDRESS,
                                    "alice@corp.example.com",
                                ),
                            ],
                        )],
                    ),
                ],
            ),
        ],
    );
    let parsed = parse_settings_user_information_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.user_information_status, Some(1));
    assert_eq!(
        parsed.email_addresses,
        vec![
            "alice@example.com".to_string(),
            "alice@corp.example.com".to_string()
        ]
    );
}

/// A command-level rejection — top-level Status only, no UserInformation
/// element (e.g. 142 "policy required" before provisioning) — surfaces on
/// `status`; `user_information_status` stays None and no addresses parse.
#[test]
fn settings_user_information_response_command_level_error() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "142")],
    );
    let parsed = parse_settings_user_information_response(&tree).expect("parse");
    assert_eq!(parsed.status, 142);
    assert_eq!(parsed.user_information_status, None);
    assert!(parsed.email_addresses.is_empty());
}

/// Nested-Status rule (mirrors the ItemOperations parser): the more
/// specific UserInformation-level Status overrides the top-level Status
/// when both are present, while both remain surfaced — the specific one
/// via `user_information_status`.
#[test]
fn settings_user_information_nested_status_overrides_top_level() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::USER_INFORMATION,
                vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "165")],
            ),
        ],
    );
    let parsed = parse_settings_user_information_response(&tree).expect("parse");
    assert_eq!(parsed.user_information_status, Some(165));
    assert_eq!(parsed.status, 165); // more specific wins
    assert!(parsed.email_addresses.is_empty());
}

/// Absent Status elements default `status` to 1 (success), mirroring the
/// GetItemEstimate pattern; `user_information_status` stays None.
#[test]
fn settings_user_information_response_defaults_status_when_absent() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(pages::SETTINGS, settings::SETTINGS, vec![]);
    let parsed = parse_settings_user_information_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.user_information_status, None);
    assert!(parsed.email_addresses.is_empty());
}
