// SPDX-License-Identifier: MPL-2.0
//! Settings DeviceInformation request shape and the generic two-status response parse.

use super::*;

#[test]
fn build_settings_device_information_tree_is_spec_shaped() {
    let tree = build_settings_device_information_request(
        "KylinsMail",
        "Kylins Mail desktop",
        "windows",
        "en-US",
    );
    use provider_eas::wbxml::tags::{pages, settings};
    assert_eq!(
        (tree.page, tree.token),
        (pages::SETTINGS, settings::SETTINGS)
    );
    let di = &tree.children[0];
    assert_eq!(
        (di.page, di.token),
        (pages::SETTINGS, settings::DEVICE_INFORMATION)
    );
    let set = &di.children[0];
    assert_eq!((set.page, set.token), (pages::SETTINGS, settings::SET));
    let tokens: Vec<u8> = set.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![
            settings::MODEL,
            settings::FRIENDLY_NAME,
            settings::OS,
            settings::OS_LANGUAGE
        ]
    );
}

#[test]
fn parse_settings_response_reads_both_statuses() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::DEVICE_INFORMATION,
                vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1")],
            ),
        ],
    );
    assert_eq!(parse_settings_response(&tree).expect("parse"), (1, 1));
}

#[test]
fn parse_settings_response_defaults_missing_statuses_to_success() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(pages::SETTINGS, settings::SETTINGS, vec![]);
    assert_eq!(parse_settings_response(&tree).expect("parse"), (1, 1));
}
