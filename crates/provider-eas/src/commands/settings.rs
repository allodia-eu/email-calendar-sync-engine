// SPDX-License-Identifier: MPL-2.0
use super::*;

// ============================================================================
// Settings (DeviceInformation)
// ============================================================================

/// Build the `<DeviceInformation><Set>…</Set></DeviceInformation>` subtree
/// (Settings code page 18). Shared by the standalone Settings command and by
/// the Provision phase-1 request, which embeds this subtree as its first
/// child per MS-ASPROV (Exchange requires DI before it will evaluate policy —
/// status 165 otherwise).
pub fn device_information_element(
    model: &str,
    friendly_name: &str,
    os: &str,
    os_language: &str,
) -> WbxmlElement {
    use crate::wbxml::tags::{pages, settings};
    WbxmlElement::container(
        pages::SETTINGS,
        settings::DEVICE_INFORMATION,
        vec![WbxmlElement::container(
            pages::SETTINGS,
            settings::SET,
            vec![
                WbxmlElement::text(pages::SETTINGS, settings::MODEL, model),
                WbxmlElement::text(pages::SETTINGS, settings::FRIENDLY_NAME, friendly_name),
                WbxmlElement::text(pages::SETTINGS, settings::OS, os),
                WbxmlElement::text(pages::SETTINGS, settings::OS_LANGUAGE, os_language),
            ],
        )],
    )
}

/// Build a Settings → DeviceInformation → Set request (MS-ASCMD §2.2.1.18).
/// Exchange may refuse Provision with status 165 (DeviceInformationRequired)
/// until the client identifies itself this way.
pub fn build_settings_device_information_request(
    model: &str,
    friendly_name: &str,
    os: &str,
    os_language: &str,
) -> WbxmlElement {
    use crate::wbxml::tags::{pages, settings};
    WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![device_information_element(
            model,
            friendly_name,
            os,
            os_language,
        )],
    )
}

/// Parse a Settings response. Returns (top-level Status, DeviceInformation
/// Status); each defaults to 1 (success) when its element is absent.
pub fn parse_settings_response(root: &WbxmlElement) -> Result<(u32, u32), WbxmlError> {
    use crate::wbxml::tags::{pages, settings};
    expect_tag(root, pages::SETTINGS, settings::SETTINGS)?;
    let mut top = 1u32;
    let mut di = 1u32;
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::STATUS {
            top = text_value(child).unwrap_or_default().parse().unwrap_or(1);
        } else if child.page == pages::SETTINGS && child.token == settings::DEVICE_INFORMATION {
            for c in &child.children {
                if c.page == pages::SETTINGS && c.token == settings::STATUS {
                    di = text_value(c).unwrap_or_default().parse().unwrap_or(1);
                }
            }
        }
    }
    Ok((top, di))
}

/// Build a Settings → UserInformation → Get request (the Get form of the
/// Settings command, [MS-ASCMD] §4.21; Settings code page 18 per
/// [MS-ASWBXML] §2.1.2.1.19):
/// ```xml
/// <Settings>               <!-- page 18, 0x05 -->
///   <UserInformation>      <!-- page 18, 0x1D -->
///     <Get/>               <!-- page 18, 0x07 — empty element -->
///   </UserInformation>
/// </Settings>
/// ```
/// The server answers with the account's SMTP addresses (used to confirm the
/// authenticated identity / feed account setup).
pub fn build_settings_user_information_request() -> WbxmlElement {
    use crate::wbxml::tags::{pages, settings};
    WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::container(
            pages::SETTINGS,
            settings::USER_INFORMATION,
            vec![WbxmlElement::empty(pages::SETTINGS, settings::GET)],
        )],
    )
}

/// Parse a Settings → UserInformation response ([MS-ASCMD] §4.21):
/// ```xml
/// <Settings>                      <!-- page 18, 0x05 -->
///   <Status>1</Status>            <!-- page 18, 0x06 — command-level -->
///   <UserInformation>             <!-- page 18, 0x1D -->
///     <Status>1</Status>          <!-- page 18, 0x06 — element-level -->
///     <Get>                       <!-- page 18, 0x07 -->
///       <EmailAddresses>          <!-- page 18, 0x1E -->
///         <SMTPAddress>…</>       <!-- page 18, 0x1F (1..n) -->
///       </EmailAddresses>
///     </Get>
///   </UserInformation>
/// </Settings>
/// ```
/// Nested-Status rule mirrors the ItemOperations parser: the top-level
/// settings:Status is read first (command-level rejection, e.g. 142 policy
/// required — then no UserInformation element is present at all), and the
/// UserInformation-level Status overrides it when present (more specific
/// wins). Both stay surfaced: the specific one on `user_information_status`.
/// A missing Status defaults to 1 (success), mirroring GetItemEstimate.
pub fn parse_settings_user_information_response(
    root: &WbxmlElement,
) -> Result<UserInformationResult, WbxmlError> {
    use crate::wbxml::tags::{pages, settings};
    expect_tag(root, pages::SETTINGS, settings::SETTINGS)?;
    let mut result = UserInformationResult {
        status: 1, // success default when Status elements are absent
        ..UserInformationResult::default()
    };
    // Top-level Status first; a UserInformation-level Status below overrides
    // it (same ordering as parse_item_operations_response).
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = match raw.parse() {
                Ok(n) => n,
                Err(_) => {
                    log::warn!(
                        "Settings UserInformation: malformed top-level Status \"{raw}\"; defaulting to 1"
                    );
                    1
                }
            };
        }
    }
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::USER_INFORMATION {
            for ui_child in &child.children {
                match (ui_child.page, ui_child.token) {
                    (pages::SETTINGS, settings::STATUS) => {
                        let raw = text_value(ui_child).unwrap_or_default();
                        let n: u32 = match raw.parse() {
                            Ok(n) => n,
                            Err(_) => {
                                log::warn!(
                                    "Settings UserInformation: malformed UserInformation Status \"{raw}\"; defaulting to 1"
                                );
                                1
                            }
                        };
                        result.user_information_status = Some(n);
                        result.status = n; // more specific wins
                    }
                    (pages::SETTINGS, settings::GET) => {
                        for get_child in &ui_child.children {
                            if get_child.page == pages::SETTINGS
                                && get_child.token == settings::EMAIL_ADDRESSES
                            {
                                for addr in &get_child.children {
                                    if addr.page == pages::SETTINGS
                                        && addr.token == settings::SMTP_ADDRESS
                                    {
                                        match text_value(addr) {
                                            Ok(s) => result.email_addresses.push(s),
                                            Err(e) => {
                                                // Non-UTF-8 SMTPAddress is malformed
                                                // server data; skip the entry but
                                                // never drop it silently.
                                                log::warn!(
                                                    "Settings UserInformation: skipping undecodable SMTPAddress entry: {e}"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(result)
}

/// Build a Settings → DevicePassword → Set request (the Set form of the
/// Settings command, [MS-ASCMD] §4.22; Settings code page 18 per
/// [MS-ASWBXML] §2.1.2.1.19):
/// ```xml
/// <Settings>               <!-- page 18, 0x05 -->
///   <DevicePassword>       <!-- page 18, 0x14 -->
///     <Set>                <!-- page 18, 0x08 -->
///       <Password>…</>     <!-- page 18, 0x15 — required (§2.2.3.132.3) -->
///     </Set>
///   </DevicePassword>
/// </Settings>
/// ```
/// The server stores the recovery password in the user's mailbox so it can
/// be recovered if the device password is forgotten. SECURITY: the password
/// travels to the server over TLS; callers and this module must NEVER log
/// it. An empty `password` clears a stored recovery password — for that case
/// §2.2.3.132.3 mandates an EMPTY Password element, so we emit `<Password/>`
/// rather than an empty text node.
pub fn build_settings_device_password_request(password: &str) -> WbxmlElement {
    use crate::wbxml::tags::{pages, settings};
    let password_el = if password.is_empty() {
        WbxmlElement::empty(pages::SETTINGS, settings::PASSWORD)
    } else {
        WbxmlElement::text(pages::SETTINGS, settings::PASSWORD, password)
    };
    WbxmlElement::container(
        pages::SETTINGS,
        settings::SETTINGS,
        vec![WbxmlElement::container(
            pages::SETTINGS,
            settings::DEVICE_PASSWORD,
            vec![WbxmlElement::container(
                pages::SETTINGS,
                settings::SET,
                vec![password_el],
            )],
        )],
    )
}

/// Parse a Settings → DevicePassword Set response ([MS-ASCMD] §4.22.2):
/// ```xml
/// <Settings>                      <!-- page 18, 0x05 -->
///   <Status>1</Status>            <!-- page 18, 0x06 — command-level -->
///   <DevicePassword>              <!-- page 18, 0x14 -->
///     <Set>                       <!-- page 18, 0x08 -->
///       <Status>…</Status>        <!-- page 18, 0x06 — element-level -->
///     </Set>
///   </DevicePassword>
/// </Settings>
/// ```
/// Nested-Status rule mirrors the UserInformation parser: the top-level
/// settings:Status is read first (command-level rejection, e.g. 142 device
/// not provisioned — then no DevicePassword element is present at all), and
/// the DevicePassword-level Status overrides it when present (more specific
/// wins). Both stay surfaced: the specific one on `device_password_status`.
/// A missing Status defaults to 1 (success), mirroring GetItemEstimate.
/// The element-level Status is accepted both nested under Set (the §4.22.2
/// wire example) and directly under DevicePassword (the parent listed in
/// §2.2.3.177.15 and the response-child table of §2.2.3.46) — the spec
/// shows both shapes, servers differ, and we must not fail a real response
/// over a spec ambiguity. DevicePassword Set element-level statuses
/// (§4.22): 1 success, 2 protocol error, 5 invalid arguments (password too
/// long), 7 denied by policy (password recovery disabled).
pub fn parse_settings_device_password_response(
    root: &WbxmlElement,
) -> Result<DevicePasswordResult, WbxmlError> {
    use crate::wbxml::tags::{pages, settings};
    expect_tag(root, pages::SETTINGS, settings::SETTINGS)?;
    let mut result = DevicePasswordResult {
        status: 1, // success default when Status elements are absent
        ..DevicePasswordResult::default()
    };
    // Top-level Status first; a DevicePassword-level Status below overrides
    // it (same ordering as parse_settings_user_information_response).
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = match raw.parse() {
                Ok(n) => n,
                Err(_) => {
                    log::warn!(
                        "Settings DevicePassword: malformed top-level Status \"{raw}\"; defaulting to 1"
                    );
                    1
                }
            };
        }
    }
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::DEVICE_PASSWORD {
            for dp_child in &child.children {
                match (dp_child.page, dp_child.token) {
                    // §2.2.3.177.15: Status may sit directly under DevicePassword.
                    (pages::SETTINGS, settings::STATUS) => {
                        let raw = text_value(dp_child).unwrap_or_default();
                        let n: u32 = match raw.parse() {
                            Ok(n) => n,
                            Err(_) => {
                                log::warn!(
                                    "Settings DevicePassword: malformed DevicePassword Status \"{raw}\"; defaulting to 1"
                                );
                                1
                            }
                        };
                        result.device_password_status = Some(n);
                        result.status = n; // more specific wins
                    }
                    // §4.22.2 wire example: Status nested under DevicePassword/Set.
                    (pages::SETTINGS, settings::SET) => {
                        for set_child in &dp_child.children {
                            if set_child.page == pages::SETTINGS
                                && set_child.token == settings::STATUS
                            {
                                let raw = text_value(set_child).unwrap_or_default();
                                let n: u32 = match raw.parse() {
                                    Ok(n) => n,
                                    Err(_) => {
                                        log::warn!(
                                            "Settings DevicePassword: malformed DevicePassword Set Status \"{raw}\"; defaulting to 1"
                                        );
                                        1
                                    }
                                };
                                result.device_password_status = Some(n);
                                result.status = n; // more specific wins
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(result)
}

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
            status = match raw.parse() {
                Ok(n) => n,
                Err(_) => {
                    log::warn!(
                        "Settings Oof Get: malformed top-level Status \"{raw}\"; defaulting to 1"
                    );
                    1
                }
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
                    let n: u32 = match raw.parse() {
                        Ok(n) => n,
                        Err(_) => {
                            log::warn!(
                                "Settings Oof Get: malformed Oof Status \"{raw}\"; defaulting to 1"
                            );
                            1
                        }
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
                applies_to = Some(OofAppliesTo::ExternalUnknown)
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
            result.status = match raw.parse() {
                Ok(n) => n,
                Err(_) => {
                    log::warn!(
                        "Settings Oof Set: malformed top-level Status \"{raw}\"; defaulting to 1"
                    );
                    1
                }
            };
        }
    }
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::OOF {
            for oof_child in &child.children {
                if oof_child.page == pages::SETTINGS && oof_child.token == settings::STATUS {
                    let raw = text_value(oof_child).unwrap_or_default();
                    let n: u32 = match raw.parse() {
                        Ok(n) => n,
                        Err(_) => {
                            log::warn!(
                                "Settings Oof Set: malformed Oof Status \"{raw}\"; defaulting to 1"
                            );
                            1
                        }
                    };
                    result.oof_status = Some(n);
                    result.status = n; // more specific wins
                }
            }
        }
    }
    Ok(result)
}
