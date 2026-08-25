// SPDX-License-Identifier: MPL-2.0
//! Settings Oof requests (Get + Set forms, [MS-ASCMD] §4.19).

use super::*;

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
