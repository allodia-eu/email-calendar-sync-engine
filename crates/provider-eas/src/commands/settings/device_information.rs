// SPDX-License-Identifier: MPL-2.0
// Settings DeviceInformation set + response parse ([MS-ASCMD] §2.2.1.18).

use crate::commands::{WbxmlElement, WbxmlError, expect_tag, text_value};

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
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
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
