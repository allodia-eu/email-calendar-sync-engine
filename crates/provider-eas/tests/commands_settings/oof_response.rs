// SPDX-License-Identifier: MPL-2.0
//! Settings Oof responses (Get + Set forms, [MS-ASCMD] §4.19).

use super::*;

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
// code_pages/pages_10_17.rs VALIDATE_TOKENS: ValidateCert=0x05, Certificates=0x06,
// Certificate=0x07, CertificateChain=0x08, CheckCrl=0x09, Status=0x0A.
// Request shape per §4.20.1 / §6.47; response shape per §4.20.2 / §6.48.
// Certificate values are opaque base64 DER payloads — the tests use
// truncated dummy strings, never real certificate material.
