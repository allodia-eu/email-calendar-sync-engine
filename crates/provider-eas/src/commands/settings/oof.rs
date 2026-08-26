// SPDX-License-Identifier: MPL-2.0
// Settings OOF get/set ([MS-ASCMD] §4.19): builds, parses, OofMessage decoding.

use crate::commands::{
    OofAppliesTo, OofMessage, OofResult, OofSettings, WbxmlElement, WbxmlError, expect_tag,
    text_value,
};

/// Build a Settings → Oof → Get request ([MS-ASCMD] §4.19.1.1; Settings
/// code page 18 per [MS-ASWBXML] §2.1.2.1.19):
/// ```xml
/// <Settings>               <!-- page 18, 0x05 -->
///   <Oof>                  <!-- page 18, 0x09 -->
///     <Get>                <!-- page 18, 0x07 -->
///       <BodyType>…</>     <!-- page 18, 0x13 — "Text" or "HTML" -->
///     </Get>
///   </Oof>
/// </Settings>
/// ```
/// BodyType is the only child of Get in an Oof Get request (§2.2.3.83); the
/// server returns all OOF settings and messages formatted for that body
/// type. SECURITY: the Get RESPONSE carries the user's OOF reply messages —
/// private content that must never be logged (see `client.rs` body-dump
/// redaction).
pub fn build_settings_oof_get_request(body_type: &str) -> WbxmlElement {
    use crate::wbxml::tags::{pages, settings};
    WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::container(
            pages::SETTINGS,
            settings::OOF,
            vec![WbxmlElement::container(
                pages::SETTINGS,
                settings::GET,
                vec![WbxmlElement::text(
                    pages::SETTINGS,
                    settings::BODY_TYPE,
                    body_type,
                )],
            )],
        )],
    )
}

/// Build a Settings → Oof → Set request ([MS-ASCMD] §4.19.2.1):
/// ```xml
/// <Settings>               <!-- page 18, 0x05 -->
///   <Oof>                  <!-- page 18, 0x09 -->
///     <Set>                <!-- page 18, 0x08 -->
///       <OofState>2</>     <!-- page 18, 0x0A — 0/1/2 (§2.2.3.124) -->
///       <StartTime/>       <!-- page 18, 0x0B — only when scheduled -->
///       <EndTime/>         <!-- page 18, 0x0C — only when scheduled -->
///       <OofMessage>       <!-- page 18, 0x0D — 0..3 -->
///         <AppliesTo…/>    <!-- audience marker, empty element -->
///         <Enabled/>       <!-- "1"/"0" -->
///         <ReplyMessage/>  <!-- optional -->
///         <BodyType/>      <!-- optional -->
///       </OofMessage>
///     </Set>
///   </Oof>
/// </Settings>
/// ```
/// Child order follows the schemas: Set emits OofState, StartTime, EndTime,
/// then the OofMessage elements (§2.2.3.167); each OofMessage emits its
/// AppliesTo* marker, then Enabled, ReplyMessage, BodyType (§2.2.3.123) —
/// absent optionals are omitted, so an enabled-only external message
/// (the §4.19.2.1 form) is just marker + Enabled. SECURITY:
/// `settings.messages[].reply_message` is private user content; callers and
/// this module must NEVER log it.
pub fn build_settings_oof_set_request(settings: &OofSettings) -> WbxmlElement {
    use crate::wbxml::tags::{pages, settings as st};
    let mut set_children: Vec<WbxmlElement> = Vec::new();
    if let Some(state) = settings.state {
        set_children.push(WbxmlElement::text(
            pages::SETTINGS,
            st::OOF_STATE,
            state.to_string(),
        ));
    }
    if let Some(start) = &settings.start_time {
        set_children.push(WbxmlElement::text(
            pages::SETTINGS,
            st::START_TIME,
            start.clone(),
        ));
    }
    if let Some(end) = &settings.end_time {
        set_children.push(WbxmlElement::text(
            pages::SETTINGS,
            st::END_TIME,
            end.clone(),
        ));
    }
    for message in &settings.messages {
        let marker = match message.applies_to {
            OofAppliesTo::Internal => st::APPLIES_TO_INTERNAL,
            OofAppliesTo::ExternalKnown => st::APPLIES_TO_EXTERNAL_KNOWN,
            OofAppliesTo::ExternalUnknown => st::APPLIES_TO_EXTERNAL_UNKNOWN,
        };
        let mut msg_children = vec![WbxmlElement::empty(pages::SETTINGS, marker)];
        if let Some(enabled) = message.enabled {
            msg_children.push(WbxmlElement::text(
                pages::SETTINGS,
                st::ENABLED,
                if enabled { "1" } else { "0" },
            ));
        }
        if let Some(reply) = &message.reply_message {
            // Empty string → empty element, mirroring the DevicePassword
            // clear-form convention (§2.2.3.132.3 style).
            msg_children.push(if reply.is_empty() {
                WbxmlElement::empty(pages::SETTINGS, st::REPLY_MESSAGE)
            } else {
                WbxmlElement::text(pages::SETTINGS, st::REPLY_MESSAGE, reply.clone())
            });
        }
        if let Some(body_type) = &message.body_type {
            msg_children.push(if body_type.is_empty() {
                WbxmlElement::empty(pages::SETTINGS, st::BODY_TYPE)
            } else {
                WbxmlElement::text(pages::SETTINGS, st::BODY_TYPE, body_type.clone())
            });
        }
        set_children.push(WbxmlElement::container(
            pages::SETTINGS,
            st::OOF_MESSAGE,
            msg_children,
        ));
    }
    WbxmlElement::container(
        pages::SETTINGS,
        st::SETTINGS,
        vec![WbxmlElement::container(
            pages::SETTINGS,
            st::OOF,
            vec![WbxmlElement::container(
                pages::SETTINGS,
                st::SET,
                set_children,
            )],
        )],
    )
}

/// Parse a Settings → Oof Get response ([MS-ASCMD] §4.19.1.2):
/// ```xml
/// <Settings>                      <!-- page 18, 0x05 -->
///   <Status>1</Status>            <!-- page 18, 0x06 — command-level -->
///   <Oof>                         <!-- page 18, 0x09 -->
///     <Status>1</Status>          <!-- page 18, 0x06 — element-level -->
///     <Get>                       <!-- page 18, 0x07 -->
///       <OofState>2</>            <!-- page 18, 0x0A -->
///       <StartTime/> <EndTime/>   <!-- page 18, 0x0B/0x0C (ISO-8601) -->
///       <OofMessage>…</>          <!-- page 18, 0x0D — one per audience -->
///     </Get>
///   </Oof>
/// </Settings>
/// ```
/// Returns `(settings, effective_status)`. Nested-Status rule mirrors the
/// UserInformation/DevicePassword parsers: the top-level settings:Status is
/// read first (command-level rejection, e.g. 142 policy required — then no
/// Oof element is present at all), and the Oof-level Status overrides it
/// when present (more specific wins). A missing Status defaults to 1
/// (success), mirroring GetItemEstimate. OofState/StartTime/EndTime and
/// each OofMessage are parsed into [`OofSettings`]; an OofMessage with no
/// recognized AppliesTo* marker is warn-logged and skipped (never
/// attributed by guessing, never swallowed). Oof Get operation statuses
/// (§2.2.3.177.15): 1 success, 2 protocol error, 5 invalid arguments,
/// 6 conflicting arguments.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_settings_oof_get_response(
    root: &WbxmlElement,
) -> Result<(OofSettings, u32), WbxmlError> {
    use crate::wbxml::tags::{pages, settings};
    expect_tag(root, pages::SETTINGS, settings::SETTINGS)?;
    let mut oof = OofSettings::default();
    let mut status = 1u32; // success default when Status elements are absent
    // Top-level Status first; an Oof-level Status below overrides it (same
    // ordering as parse_settings_user_information_response).
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::STATUS {
            let raw = text_value(child).unwrap_or_default();
            status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!(
                    "Settings Oof Get: malformed top-level Status \"{raw}\"; defaulting to 1"
                );
                1
            };
        }
    }
    for child in &root.children {
        if child.page != pages::SETTINGS || child.token != settings::OOF {
            continue;
        }
        for oof_child in &child.children {
            match (oof_child.page, oof_child.token) {
                (pages::SETTINGS, settings::STATUS) => {
                    let raw = text_value(oof_child).unwrap_or_default();
                    let n: u32 = if let Ok(n) = raw.parse() {
                        n
                    } else {
                        log::warn!(
                            "Settings Oof Get: malformed Oof Status \"{raw}\"; defaulting to 1"
                        );
                        1
                    };
                    status = n; // more specific wins
                }
                (pages::SETTINGS, settings::GET) => {
                    for get_child in &oof_child.children {
                        match (get_child.page, get_child.token) {
                            (pages::SETTINGS, settings::OOF_STATE) => {
                                let raw = text_value(get_child).unwrap_or_default();
                                match raw.parse() {
                                    Ok(n) => oof.state = Some(n),
                                    Err(_) => {
                                        // Malformed OofState stays None rather
                                        // than guessing 0/1/2 — but never
                                        // silently.
                                        log::warn!(
                                            "Settings Oof Get: malformed OofState \"{raw}\"; leaving state unset"
                                        );
                                    }
                                }
                            }
                            (pages::SETTINGS, settings::START_TIME) => {
                                match text_value(get_child) {
                                    Ok(s) => oof.start_time = Some(s),
                                    Err(e) => {
                                        log::warn!(
                                            "Settings Oof Get: skipping undecodable StartTime: {e}"
                                        );
                                    }
                                }
                            }
                            (pages::SETTINGS, settings::END_TIME) => match text_value(get_child) {
                                Ok(s) => oof.end_time = Some(s),
                                Err(e) => {
                                    log::warn!(
                                        "Settings Oof Get: skipping undecodable EndTime: {e}"
                                    );
                                }
                            },
                            (pages::SETTINGS, settings::OOF_MESSAGE) => {
                                if let Some(message) = parse_oof_message(get_child) {
                                    oof.messages.push(message);
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
    Ok((oof, status))
}

/// Parse one settings:OofMessage element ([MS-ASCMD] §2.2.3.123) into an
/// [`OofMessage`]. Returns `None` — after warn-logging — when no recognized
/// AppliesTo* audience marker is present: §2.2.3.123 ties every OofMessage
/// to exactly one audience, and attributing it by position would be a
/// guess. Never swallowed: the skip is always logged.
fn parse_oof_message(elem: &WbxmlElement) -> Option<OofMessage> {
    use crate::wbxml::tags::{pages, settings};
    let mut applies_to: Option<OofAppliesTo> = None;
    let mut enabled: Option<bool> = None;
    let mut reply_message: Option<String> = None;
    let mut body_type: Option<String> = None;
    for child in &elem.children {
        if child.page != pages::SETTINGS {
            continue;
        }
        match child.token {
            settings::APPLIES_TO_INTERNAL => applies_to = Some(OofAppliesTo::Internal),
            settings::APPLIES_TO_EXTERNAL_KNOWN => applies_to = Some(OofAppliesTo::ExternalKnown),
            settings::APPLIES_TO_EXTERNAL_UNKNOWN => {
                applies_to = Some(OofAppliesTo::ExternalUnknown);
            }
            settings::ENABLED => {
                let raw = text_value(child).unwrap_or_default();
                enabled = match raw.as_str() {
                    // §2.2.3.59: only "1" and "0" are valid.
                    "1" => Some(true),
                    "0" => Some(false),
                    other => {
                        log::warn!(
                            "Settings Oof: malformed Enabled value \"{other}\" (expected \"1\" or \"0\"); leaving it unset"
                        );
                        None
                    }
                };
            }
            settings::REPLY_MESSAGE => match text_value(child) {
                Ok(s) => reply_message = Some(s),
                Err(e) => {
                    log::warn!("Settings Oof: skipping undecodable ReplyMessage: {e}");
                }
            },
            settings::BODY_TYPE => match text_value(child) {
                Ok(s) => body_type = Some(s),
                Err(e) => {
                    log::warn!("Settings Oof: skipping undecodable BodyType: {e}");
                }
            },
            _ => {}
        }
    }
    let Some(applies_to) = applies_to else {
        log::warn!(
            "Settings Oof: skipping OofMessage without a recognized AppliesTo* marker ({} children)",
            elem.children.len()
        );
        return None;
    };
    Some(OofMessage {
        applies_to,
        enabled,
        reply_message,
        body_type,
    })
}

/// Parse a Settings → Oof Set response ([MS-ASCMD] §4.19.2.2):
/// ```xml
/// <Settings>                      <!-- page 18, 0x05 -->
///   <Status>1</Status>            <!-- page 18, 0x06 — command-level -->
///   <Oof>                         <!-- page 18, 0x09 -->
///     <Status>1</Status>          <!-- page 18, 0x06 — element-level -->
///   </Oof>
/// </Settings>
/// ```
/// Nested-Status rule mirrors the UserInformation/DevicePassword parsers:
/// the top-level settings:Status is read first (command-level rejection,
/// e.g. 142 device not provisioned — then no Oof element is present at
/// all), and the Oof-level Status overrides it when present (more specific
/// wins). Both stay surfaced: the specific one on `oof_status`. A missing
/// Status defaults to 1 (success), mirroring GetItemEstimate. The
/// element-level Status sits directly under Oof — the §4.19.2.2 wire shape
/// and the only parent §2.2.3.177.15 lists for it (unlike DevicePassword,
/// the spec never nests it under Oof/Set). Oof Set operation statuses
/// (§2.2.3.177.15): 1 success, 2 protocol error, 5 invalid arguments,
/// 6 conflicting arguments.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_settings_oof_set_response(root: &WbxmlElement) -> Result<OofResult, WbxmlError> {
    use crate::wbxml::tags::{pages, settings};
    expect_tag(root, pages::SETTINGS, settings::SETTINGS)?;
    let mut result = OofResult {
        status: 1, // success default when Status elements are absent
        ..OofResult::default()
    };
    // Top-level Status first; an Oof-level Status below overrides it (same
    // ordering as parse_settings_user_information_response).
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!(
                    "Settings Oof Set: malformed top-level Status \"{raw}\"; defaulting to 1"
                );
                1
            };
        }
    }
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::OOF {
            for oof_child in &child.children {
                if oof_child.page == pages::SETTINGS && oof_child.token == settings::STATUS {
                    let raw = text_value(oof_child).unwrap_or_default();
                    let n: u32 = if let Ok(n) = raw.parse() {
                        n
                    } else {
                        log::warn!(
                            "Settings Oof Set: malformed Oof Status \"{raw}\"; defaulting to 1"
                        );
                        1
                    };
                    result.oof_status = Some(n);
                    result.status = n; // more specific wins
                }
            }
        }
    }
    Ok(result)
}
