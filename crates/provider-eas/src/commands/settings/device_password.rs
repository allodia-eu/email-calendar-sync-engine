// SPDX-License-Identifier: MPL-2.0
// Settings DevicePassword set ([MS-ASCMD] §4.22).

use crate::commands::{DevicePasswordResult, WbxmlElement, WbxmlError, expect_tag, text_value};

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
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
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
            result.status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!(
                    "Settings DevicePassword: malformed top-level Status \"{raw}\"; defaulting to 1"
                );
                1
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
                        let n: u32 = if let Ok(n) = raw.parse() {
                            n
                        } else {
                            log::warn!(
                                "Settings DevicePassword: malformed DevicePassword Status \"{raw}\"; defaulting to 1"
                            );
                            1
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
                                let n: u32 = if let Ok(n) = raw.parse() {
                                    n
                                } else {
                                    log::warn!(
                                        "Settings DevicePassword: malformed DevicePassword Set Status \"{raw}\"; defaulting to 1"
                                    );
                                    1
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
