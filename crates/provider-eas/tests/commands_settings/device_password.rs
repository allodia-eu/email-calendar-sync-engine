// SPDX-License-Identifier: MPL-2.0
//! Settings DevicePassword (Set form, [MS-ASCMD] §4.22).

use super::*;

// ---- Settings DevicePassword (Set form, [MS-ASCMD] §4.22) ----

/// Request shape per [MS-ASCMD] §4.22.1 and [MS-ASWBXML] §2.1.2.1.19
/// (Settings code page 18):
/// ```text
/// Settings (18,0x05) > DevicePassword (18,0x14) > Set (18,0x08) >
///   Password (18,0x15) = "bar"
/// ```
/// `Password` is the only child of Set and is required (§2.2.3.132.3).
#[test]
fn settings_device_password_request_uses_spec_shape() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = build_settings_device_password_request("bar");
    assert_eq!(
        (tree.page, tree.token),
        (pages::SETTINGS, settings::SETTINGS)
    );
    assert_eq!(tree.children.len(), 1);
    let dp = &tree.children[0];
    assert_eq!(
        (dp.page, dp.token),
        (pages::SETTINGS, settings::DEVICE_PASSWORD)
    );
    assert_eq!(dp.children.len(), 1);
    let set = &dp.children[0];
    assert_eq!((set.page, set.token), (pages::SETTINGS, settings::SET));
    assert_eq!(set.children.len(), 1);
    let pw = &set.children[0];
    assert_eq!((pw.page, pw.token), (pages::SETTINGS, settings::PASSWORD));
    assert!(matches!(&pw.value, WbxmlValue::Text(t) if t == "bar"));
}

#[test]
fn settings_device_password_request_round_trips() {
    let tree = build_settings_device_password_request("recovery-Passw0rd");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// [MS-ASCMD] §2.2.3.132.3: to CLEAR a stored recovery password the
/// client MUST send the DevicePassword Set request with an EMPTY Password
/// element — so an empty input string builds `<Password/>`, not an empty
/// text node.
#[test]
fn settings_device_password_clear_sends_empty_password_element() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = build_settings_device_password_request("");
    assert_eq!(tree.children.len(), 1);
    let pw = &tree.children[0].children[0].children[0];
    assert_eq!((pw.page, pw.token), (pages::SETTINGS, settings::PASSWORD));
    assert!(matches!(pw.value, WbxmlValue::Empty));
}

/// Response shape per [MS-ASCMD] §4.22.2:
/// ```text
/// Settings (18,0x05) > Status (18,0x06), DevicePassword (18,0x14) >
///   Set (18,0x08) > Status (18,0x06)
/// ```
/// Both Status levels are surfaced: the effective one on `status`, the
/// specific one on `device_password_status`.
#[test]
fn settings_device_password_response_parses_both_statuses() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::DEVICE_PASSWORD,
                vec![WbxmlElement::container(
                    pages::SETTINGS,
                    settings::SET,
                    vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1")],
                )],
            ),
        ],
    );
    let parsed = parse_settings_device_password_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.device_password_status, Some(1));
}

/// A command-level rejection — top-level Status only, no DevicePassword
/// element (e.g. 142 "device not provisioned") — surfaces on `status`;
/// `device_password_status` stays None.
#[test]
fn settings_device_password_response_command_level_error() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "142")],
    );
    let parsed = parse_settings_device_password_response(&tree).expect("parse");
    assert_eq!(parsed.status, 142);
    assert_eq!(parsed.device_password_status, None);
}

/// Nested-Status rule (mirrors the UserInformation/ItemOperations
/// parsers): the more specific DevicePassword-level Status overrides the
/// top-level Status when both are present, while both remain surfaced —
/// the specific one via `device_password_status`. §4.22 lists the
/// DevicePassword Set statuses 1/2/5/7; 7 = "denied by policy" (admin
/// disabled password recovery).
#[test]
fn settings_device_password_nested_status_overrides_top_level() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::DEVICE_PASSWORD,
                vec![WbxmlElement::container(
                    pages::SETTINGS,
                    settings::SET,
                    vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "7")],
                )],
            ),
        ],
    );
    let parsed = parse_settings_device_password_response(&tree).expect("parse");
    assert_eq!(parsed.device_password_status, Some(7));
    assert_eq!(parsed.status, 7); // more specific wins
}

/// [MS-ASCMD] §2.2.3.177.15 names DevicePassword itself as a valid
/// parent of settings:Status (the §2.2.3.46 element table also lists
/// Status as DevicePassword's response child), while the §4.22.2 worked
/// example nests it under Set. The parser accepts both wire shapes.
#[test]
fn settings_device_password_status_accepted_directly_under_device_password() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::DEVICE_PASSWORD,
                vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "5")],
            ),
        ],
    );
    let parsed = parse_settings_device_password_response(&tree).expect("parse");
    assert_eq!(parsed.device_password_status, Some(5));
    assert_eq!(parsed.status, 5); // more specific wins
}

/// Absent Status elements default `status` to 1 (success), mirroring the
/// GetItemEstimate/UserInformation pattern; `device_password_status`
/// stays None.
#[test]
fn settings_device_password_response_defaults_status_when_absent() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(pages::SETTINGS, settings::SETTINGS, vec![]);
    let parsed = parse_settings_device_password_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.device_password_status, None);
}
