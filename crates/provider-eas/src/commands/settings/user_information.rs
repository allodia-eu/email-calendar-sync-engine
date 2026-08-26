// SPDX-License-Identifier: MPL-2.0
// Settings UserInformation get ([MS-ASCMD] §4.21).

use crate::commands::{UserInformationResult, WbxmlElement, WbxmlError, expect_tag, text_value};

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
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
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
            result.status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!(
                    "Settings UserInformation: malformed top-level Status \"{raw}\"; defaulting to 1"
                );
                1
            };
        }
    }
    for child in &root.children {
        if child.page == pages::SETTINGS && child.token == settings::USER_INFORMATION {
            for ui_child in &child.children {
                match (ui_child.page, ui_child.token) {
                    (pages::SETTINGS, settings::STATUS) => {
                        let raw = text_value(ui_child).unwrap_or_default();
                        let n: u32 = if let Ok(n) = raw.parse() {
                            n
                        } else {
                            log::warn!(
                                "Settings UserInformation: malformed UserInformation Status \"{raw}\"; defaulting to 1"
                            );
                            1
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
