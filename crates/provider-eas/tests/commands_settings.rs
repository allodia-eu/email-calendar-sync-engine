// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::{tests_common::*, *};

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

// ---- Settings Oof (Get + Set forms, [MS-ASCMD] §4.19) ----

/// Request shape per [MS-ASCMD] §4.19.1.1 and [MS-ASWBXML] §2.1.2.1.19
/// (Settings code page 18):
/// ```text
/// Settings (18,0x05) > Oof (18,0x09) > Get (18,0x07) >
///   BodyType (18,0x13) = "Text"
/// ```
/// BodyType is the only child of Get in an Oof Get request (§2.2.3.83);
/// the server returns all OOF settings and messages in that body type.
#[test]
fn settings_oof_get_request_uses_spec_shape() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = build_settings_oof_get_request("Text");
    assert_eq!(
        (tree.page, tree.token),
        (pages::SETTINGS, settings::SETTINGS)
    );
    assert_eq!(tree.children.len(), 1);
    let oof = &tree.children[0];
    assert_eq!((oof.page, oof.token), (pages::SETTINGS, settings::OOF));
    assert_eq!(oof.children.len(), 1);
    let get = &oof.children[0];
    assert_eq!((get.page, get.token), (pages::SETTINGS, settings::GET));
    assert_eq!(get.children.len(), 1);
    let body_type = &get.children[0];
    assert_eq!(
        (body_type.page, body_type.token),
        (pages::SETTINGS, settings::BODY_TYPE)
    );
    assert!(matches!(&body_type.value, WbxmlValue::Text(t) if t == "Text"));
}

#[test]
fn settings_oof_get_request_round_trips() {
    let tree = build_settings_oof_get_request("HTML");
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Request shape per [MS-ASCMD] §4.19.2.1: Settings > Oof > Set with
/// OofState first (§2.2.3.167 child order: OofState, StartTime, EndTime,
/// OofMessage*), and each OofMessage's children in §2.2.3.123 order
/// (AppliesTo* marker, Enabled, ReplyMessage, BodyType — absent
/// optionals omitted). Mirrors the §4.19.2.1 example: one fully
/// populated internal message plus two ENABLED-ONLY external messages
/// (AppliesTo marker + Enabled, no ReplyMessage/BodyType).
#[test]
fn settings_oof_set_request_uses_spec_shape() {
    use provider_eas::wbxml::tags::{pages, settings};
    let oof = OofSettings {
        state: Some(2),
        start_time: None,
        end_time: None,
        messages: vec![
            OofMessage {
                applies_to: OofAppliesTo::Internal,
                enabled: Some(true),
                reply_message: Some("I'll be out of the office today.".to_string()),
                body_type: Some("HTML".to_string()),
            },
            OofMessage {
                applies_to: OofAppliesTo::ExternalKnown,
                enabled: Some(false),
                reply_message: None,
                body_type: None,
            },
            OofMessage {
                applies_to: OofAppliesTo::ExternalUnknown,
                enabled: Some(false),
                reply_message: None,
                body_type: None,
            },
        ],
    };
    let tree = build_settings_oof_set_request(&oof);
    assert_eq!(
        (tree.page, tree.token),
        (pages::SETTINGS, settings::SETTINGS)
    );
    assert_eq!(tree.children.len(), 1);
    let oof_el = &tree.children[0];
    assert_eq!(
        (oof_el.page, oof_el.token),
        (pages::SETTINGS, settings::OOF)
    );
    assert_eq!(oof_el.children.len(), 1);
    let set = &oof_el.children[0];
    assert_eq!((set.page, set.token), (pages::SETTINGS, settings::SET));
    // OofState + three OofMessages; the absent StartTime/EndTime are omitted.
    assert_eq!(set.children.len(), 4);
    let state = &set.children[0];
    assert_eq!(
        (state.page, state.token),
        (pages::SETTINGS, settings::OOF_STATE)
    );
    assert!(matches!(&state.value, WbxmlValue::Text(t) if t == "2"));

    // Internal message: marker + Enabled + ReplyMessage + BodyType, in
    // the §2.2.3.123 order. The AppliesTo marker is an empty element
    // ("distinguished only by the presence or absence of the tag",
    // §2.2.3.14).
    let m0 = &set.children[1];
    assert_eq!(
        (m0.page, m0.token),
        (pages::SETTINGS, settings::OOF_MESSAGE)
    );
    let tokens0: Vec<u8> = m0.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens0,
        vec![
            settings::APPLIES_TO_INTERNAL,
            settings::ENABLED,
            settings::REPLY_MESSAGE,
            settings::BODY_TYPE,
        ]
    );
    assert!(matches!(m0.children[0].value, WbxmlValue::Empty));
    assert!(matches!(&m0.children[1].value, WbxmlValue::Text(t) if t == "1"));
    assert!(
        matches!(&m0.children[2].value, WbxmlValue::Text(t) if t == "I'll be out of the office today.")
    );
    assert!(matches!(&m0.children[3].value, WbxmlValue::Text(t) if t == "HTML"));

    // External known: marker + Enabled only — the §4.19.2.1 enabled-only
    // external form (no ReplyMessage, no BodyType).
    let m1 = &set.children[2];
    let tokens1: Vec<u8> = m1.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens1,
        vec![settings::APPLIES_TO_EXTERNAL_KNOWN, settings::ENABLED]
    );
    assert!(matches!(&m1.children[1].value, WbxmlValue::Text(t) if t == "0"));

    // External unknown: marker + Enabled only.
    let m2 = &set.children[3];
    let tokens2: Vec<u8> = m2.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens2,
        vec![settings::APPLIES_TO_EXTERNAL_UNKNOWN, settings::ENABLED]
    );
    assert!(matches!(&m2.children[1].value, WbxmlValue::Text(t) if t == "0"));
}

/// Scheduled OOF (§2.2.3.124: OofState MUST be 2 when StartTime/EndTime
/// are present). Set's children keep the §2.2.3.167 order: OofState,
/// StartTime, EndTime, then the OofMessage elements.
#[test]
fn settings_oof_set_request_orders_state_times_messages() {
    use provider_eas::wbxml::tags::{pages, settings};
    let oof = OofSettings {
        state: Some(2),
        start_time: Some("2026-08-06T09:00:00.000Z".to_string()),
        end_time: Some("2026-08-08T17:00:00.000Z".to_string()),
        messages: vec![OofMessage {
            applies_to: OofAppliesTo::Internal,
            enabled: Some(true),
            reply_message: None,
            body_type: None,
        }],
    };
    let tree = build_settings_oof_set_request(&oof);
    let set = &tree.children[0].children[0];
    assert_eq!((set.page, set.token), (pages::SETTINGS, settings::SET));
    let tokens: Vec<u8> = set.children.iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec![
            settings::OOF_STATE,
            settings::START_TIME,
            settings::END_TIME,
            settings::OOF_MESSAGE,
        ]
    );
    assert!(
        matches!(&set.children[1].value, WbxmlValue::Text(t) if t == "2026-08-06T09:00:00.000Z")
    );
    assert!(
        matches!(&set.children[2].value, WbxmlValue::Text(t) if t == "2026-08-08T17:00:00.000Z")
    );
}

#[test]
fn settings_oof_set_request_round_trips() {
    let oof = OofSettings {
        state: Some(2),
        start_time: Some("2026-08-06T09:00:00.000Z".to_string()),
        end_time: Some("2026-08-08T17:00:00.000Z".to_string()),
        messages: vec![OofMessage {
            applies_to: OofAppliesTo::Internal,
            enabled: Some(true),
            reply_message: Some("Away — back Monday.".to_string()),
            body_type: Some("Text".to_string()),
        }],
    };
    let tree = build_settings_oof_set_request(&oof);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Response shape per [MS-ASCMD] §4.19.1.2:
/// ```text
/// Settings (18,0x05) > Status (18,0x06), Oof (18,0x09) >
///   Status (18,0x06), Get (18,0x07) > OofState (18,0x0A),
///   StartTime (18,0x0B), EndTime (18,0x0C), OofMessage (18,0x0D) × 3
/// ```
/// One OofMessage per audience (§2.2.3.123); both Status levels surface
/// via the effective status (returned alongside the parsed settings).
#[test]
fn settings_oof_get_response_parses_spec_fixture() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::OOF,
                vec![
                    WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
                    WbxmlElement::container(
                        pages::SETTINGS,
                        settings::GET,
                        vec![
                            WbxmlElement::text(pages::SETTINGS, settings::OOF_STATE, "2"),
                            WbxmlElement::text(
                                pages::SETTINGS,
                                settings::START_TIME,
                                "2007-05-08T10:45:51.250Z",
                            ),
                            WbxmlElement::text(
                                pages::SETTINGS,
                                settings::END_TIME,
                                "2007-05-11T10:45:51.250Z",
                            ),
                            WbxmlElement::container(
                                pages::SETTINGS,
                                settings::OOF_MESSAGE,
                                vec![
                                    WbxmlElement::empty(
                                        pages::SETTINGS,
                                        settings::APPLIES_TO_INTERNAL,
                                    ),
                                    WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "1"),
                                    WbxmlElement::text(
                                        pages::SETTINGS,
                                        settings::REPLY_MESSAGE,
                                        "Internal OOF Message",
                                    ),
                                    WbxmlElement::text(
                                        pages::SETTINGS,
                                        settings::BODY_TYPE,
                                        "Text",
                                    ),
                                ],
                            ),
                            WbxmlElement::container(
                                pages::SETTINGS,
                                settings::OOF_MESSAGE,
                                vec![
                                    WbxmlElement::empty(
                                        pages::SETTINGS,
                                        settings::APPLIES_TO_EXTERNAL_KNOWN,
                                    ),
                                    WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "1"),
                                    WbxmlElement::text(
                                        pages::SETTINGS,
                                        settings::REPLY_MESSAGE,
                                        "External OOF Message",
                                    ),
                                    WbxmlElement::text(
                                        pages::SETTINGS,
                                        settings::BODY_TYPE,
                                        "Text",
                                    ),
                                ],
                            ),
                            WbxmlElement::container(
                                pages::SETTINGS,
                                settings::OOF_MESSAGE,
                                vec![
                                    WbxmlElement::empty(
                                        pages::SETTINGS,
                                        settings::APPLIES_TO_EXTERNAL_UNKNOWN,
                                    ),
                                    WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "0"),
                                    WbxmlElement::text(
                                        pages::SETTINGS,
                                        settings::REPLY_MESSAGE,
                                        "External OOF Message",
                                    ),
                                    WbxmlElement::text(
                                        pages::SETTINGS,
                                        settings::BODY_TYPE,
                                        "Text",
                                    ),
                                ],
                            ),
                        ],
                    ),
                ],
            ),
        ],
    );
    let (parsed, status) = parse_settings_oof_get_response(&tree).expect("parse");
    assert_eq!(status, 1);
    assert_eq!(parsed.state, Some(2));
    assert_eq!(
        parsed.start_time.as_deref(),
        Some("2007-05-08T10:45:51.250Z")
    );
    assert_eq!(parsed.end_time.as_deref(), Some("2007-05-11T10:45:51.250Z"));
    assert_eq!(parsed.messages.len(), 3);

    assert_eq!(parsed.messages[0].applies_to, OofAppliesTo::Internal);
    assert_eq!(parsed.messages[0].enabled, Some(true));
    assert_eq!(
        parsed.messages[0].reply_message.as_deref(),
        Some("Internal OOF Message")
    );
    assert_eq!(parsed.messages[0].body_type.as_deref(), Some("Text"));

    assert_eq!(parsed.messages[1].applies_to, OofAppliesTo::ExternalKnown);
    assert_eq!(parsed.messages[1].enabled, Some(true));
    assert_eq!(
        parsed.messages[1].reply_message.as_deref(),
        Some("External OOF Message")
    );

    assert_eq!(parsed.messages[2].applies_to, OofAppliesTo::ExternalUnknown);
    assert_eq!(parsed.messages[2].enabled, Some(false));
    assert_eq!(
        parsed.messages[2].reply_message.as_deref(),
        Some("External OOF Message")
    );
    assert_eq!(parsed.messages[2].body_type.as_deref(), Some("Text"));
}

/// A command-level rejection — top-level Status only, no Oof element
/// (e.g. 142 "policy required" before provisioning) — surfaces as the
/// effective status; the settings stay empty.
#[test]
fn settings_oof_get_response_command_level_error() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "142")],
    );
    let (parsed, status) = parse_settings_oof_get_response(&tree).expect("parse");
    assert_eq!(status, 142);
    assert_eq!(parsed.state, None);
    assert!(parsed.start_time.is_none());
    assert!(parsed.end_time.is_none());
    assert!(parsed.messages.is_empty());
}

/// Nested-Status rule (mirrors the UserInformation/DevicePassword
/// parsers): the more specific Oof-level Status overrides the top-level
/// Status when both are present. §2.2.3.177.15 names Oof as a valid
/// parent of settings:Status; Oof Get/Set operation statuses are
/// 1/2/5/6 (5 = invalid arguments).
#[test]
fn settings_oof_get_nested_status_overrides_top_level() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::OOF,
                vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "5")],
            ),
        ],
    );
    let (_parsed, status) = parse_settings_oof_get_response(&tree).expect("parse");
    assert_eq!(status, 5); // more specific wins
}

/// Absent Status elements default the effective status to 1 (success),
/// mirroring the GetItemEstimate/UserInformation pattern.
#[test]
fn settings_oof_get_response_defaults_status_when_absent() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(pages::SETTINGS, settings::SETTINGS, vec![]);
    let (parsed, status) = parse_settings_oof_get_response(&tree).expect("parse");
    assert_eq!(status, 1);
    assert_eq!(parsed.state, None);
    assert!(parsed.messages.is_empty());
}

/// An OofMessage that cannot be attributed to an audience — no
/// recognized AppliesTo* marker — is skipped with a warning (never
/// swallowed, never attributed by guessing); sibling messages and the
/// scalar fields still parse. §2.2.3.123 ties every OofMessage to
/// exactly one audience marker.
#[test]
fn settings_oof_get_response_skips_message_without_applies_to_marker() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::container(
            pages::SETTINGS,
            settings::OOF,
            vec![WbxmlElement::container(
                pages::SETTINGS,
                settings::GET,
                vec![
                    WbxmlElement::text(pages::SETTINGS, settings::OOF_STATE, "0"),
                    WbxmlElement::container(
                        pages::SETTINGS,
                        settings::OOF_MESSAGE,
                        vec![
                            WbxmlElement::empty(pages::SETTINGS, settings::APPLIES_TO_INTERNAL),
                            WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "1"),
                            WbxmlElement::text(
                                pages::SETTINGS,
                                settings::REPLY_MESSAGE,
                                "Internal OOF Message",
                            ),
                        ],
                    ),
                    // Orphan: Enabled + ReplyMessage but no AppliesTo* marker.
                    WbxmlElement::container(
                        pages::SETTINGS,
                        settings::OOF_MESSAGE,
                        vec![
                            WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "1"),
                            WbxmlElement::text(
                                pages::SETTINGS,
                                settings::REPLY_MESSAGE,
                                "orphaned message",
                            ),
                        ],
                    ),
                    WbxmlElement::container(
                        pages::SETTINGS,
                        settings::OOF_MESSAGE,
                        vec![
                            WbxmlElement::empty(
                                pages::SETTINGS,
                                settings::APPLIES_TO_EXTERNAL_UNKNOWN,
                            ),
                            WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "0"),
                        ],
                    ),
                ],
            )],
        )],
    );
    let (parsed, status) = parse_settings_oof_get_response(&tree).expect("parse");
    assert_eq!(status, 1);
    assert_eq!(parsed.state, Some(0));
    assert_eq!(parsed.messages.len(), 2); // orphan skipped
    assert_eq!(parsed.messages[0].applies_to, OofAppliesTo::Internal);
    assert_eq!(parsed.messages[1].applies_to, OofAppliesTo::ExternalUnknown);
    assert_eq!(parsed.messages[1].enabled, Some(false));
    assert_eq!(parsed.messages[1].reply_message, None);
}

/// A malformed Enabled value (§2.2.3.59: "1" or "0") is not coerced or
/// swallowed: the field stays None — logged at the parse site — and the
/// rest of the message still parses.
#[test]
fn settings_oof_get_response_malformed_enabled_stays_none() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::container(
            pages::SETTINGS,
            settings::OOF,
            vec![WbxmlElement::container(
                pages::SETTINGS,
                settings::GET,
                vec![WbxmlElement::container(
                    pages::SETTINGS,
                    settings::OOF_MESSAGE,
                    vec![
                        WbxmlElement::empty(pages::SETTINGS, settings::APPLIES_TO_INTERNAL),
                        WbxmlElement::text(pages::SETTINGS, settings::ENABLED, "maybe"),
                        WbxmlElement::text(
                            pages::SETTINGS,
                            settings::REPLY_MESSAGE,
                            "Internal OOF Message",
                        ),
                    ],
                )],
            )],
        )],
    );
    let (parsed, _status) = parse_settings_oof_get_response(&tree).expect("parse");
    assert_eq!(parsed.messages.len(), 1);
    assert_eq!(parsed.messages[0].applies_to, OofAppliesTo::Internal);
    assert_eq!(parsed.messages[0].enabled, None);
    assert_eq!(
        parsed.messages[0].reply_message.as_deref(),
        Some("Internal OOF Message")
    );
}

/// Response shape per [MS-ASCMD] §4.19.2.2:
/// ```text
/// Settings (18,0x05) > Status (18,0x06), Oof (18,0x09) > Status (18,0x06)
/// ```
/// Both Status levels are surfaced: the effective one on `status`, the
/// specific one on `oof_status`.
#[test]
fn settings_oof_set_response_parses_both_statuses() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::OOF,
                vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1")],
            ),
        ],
    );
    let parsed = parse_settings_oof_set_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.oof_status, Some(1));
}

/// A command-level rejection — top-level Status only, no Oof element
/// (e.g. 142 "device not provisioned") — surfaces on `status`;
/// `oof_status` stays None.
#[test]
fn settings_oof_set_response_command_level_error() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "142")],
    );
    let parsed = parse_settings_oof_set_response(&tree).expect("parse");
    assert_eq!(parsed.status, 142);
    assert_eq!(parsed.oof_status, None);
}

/// Nested-Status rule (mirrors the family): the more specific Oof-level
/// Status overrides the top-level Status when both are present, while
/// both remain surfaced — the specific one via `oof_status`. 6 =
/// conflicting arguments (§2.2.3.177.15 Oof Set statuses 1/2/5/6).
#[test]
fn settings_oof_set_nested_status_overrides_top_level() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            WbxmlElement::container(
                pages::SETTINGS,
                settings::OOF,
                vec![WbxmlElement::text(pages::SETTINGS, settings::STATUS, "6")],
            ),
        ],
    );
    let parsed = parse_settings_oof_set_response(&tree).expect("parse");
    assert_eq!(parsed.oof_status, Some(6));
    assert_eq!(parsed.status, 6); // more specific wins
}

/// Absent Status elements default `status` to 1 (success), mirroring the
/// GetItemEstimate/UserInformation/DevicePassword pattern; `oof_status`
/// stays None.
#[test]
fn settings_oof_set_response_defaults_status_when_absent() {
    use provider_eas::wbxml::tags::{pages, settings};
    let tree = WbxmlElement::container(pages::SETTINGS, settings::SETTINGS, vec![]);
    let parsed = parse_settings_oof_set_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.oof_status, None);
}

// ---- ValidateCert ([MS-ASCMD] §2.2.1.22, §4.20; WBXML code page 11) ----
//
// Token table verified against [MS-ASWBXML] §2.1.2.1.12 (page 11) via
// code_pages.rs VALIDATE_TOKENS: ValidateCert=0x05, Certificates=0x06,
// Certificate=0x07, CertificateChain=0x08, CheckCrl=0x09, Status=0x0A.
// Request shape per §4.20.1 / §6.47; response shape per §4.20.2 / §6.48.
// Certificate values are opaque base64 DER payloads — the tests use
// truncated dummy strings, never real certificate material.
