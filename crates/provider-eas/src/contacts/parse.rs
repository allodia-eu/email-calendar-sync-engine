// SPDX-License-Identifier: MPL-2.0
// Downsync parse of a Contacts-class ApplicationData element + field helpers.

use super::{
    model::{ContactsAddress, ContactsContactProps},
    tokens::{
        CON_ANNIVERSARY, CON_ASSISTANT_NAME, CON_ASSISTANT_PHONE, CON_BIRTHDAY,
        CON_BUSINESS_2_PHONE, CON_BUSINESS_ADDRESS_CITY, CON_BUSINESS_ADDRESS_COUNTRY,
        CON_BUSINESS_ADDRESS_POSTAL_CODE, CON_BUSINESS_ADDRESS_STATE, CON_BUSINESS_ADDRESS_STREET,
        CON_BUSINESS_FAX, CON_BUSINESS_PHONE, CON_CAR_PHONE, CON_COMPANY_NAME, CON_EMAIL_1,
        CON_EMAIL_2, CON_EMAIL_3, CON_FILE_AS, CON_FIRST_NAME, CON_HOME_2_PHONE,
        CON_HOME_ADDRESS_CITY, CON_HOME_ADDRESS_COUNTRY, CON_HOME_ADDRESS_POSTAL_CODE,
        CON_HOME_ADDRESS_STATE, CON_HOME_ADDRESS_STREET, CON_HOME_FAX, CON_HOME_PHONE,
        CON_JOB_TITLE, CON_LAST_NAME, CON_MIDDLE_NAME, CON_MOBILE_PHONE, CON_OTHER_ADDRESS_CITY,
        CON_OTHER_ADDRESS_COUNTRY, CON_OTHER_ADDRESS_POSTAL_CODE, CON_OTHER_ADDRESS_STATE,
        CON_OTHER_ADDRESS_STREET, CON_PAGER, CON_PICTURE, CON_RADIO_PHONE, CON_SUFFIX, CON_TITLE,
        CON_WEB_PAGE, CON2_COMPANY_MAIN_PHONE, CON2_MANAGER_NAME, PAGE_CONTACTS,
    },
};
use crate::wbxml::{
    WbxmlElement, WbxmlError, WbxmlValue,
    tags::{base, pages},
};

// ============================================================================
// Parse entry
// ============================================================================

/// Parse a Contacts-class `ApplicationData` element into
/// [`ContactsContactProps`].
///
/// `app_data` is the `airsync:ApplicationData` (page 0, 0x1D) child of a
/// Sync Add/Change item whose collection class is `Contacts`. Dispatch is by
/// `(page, token)`: page-1 Contacts tokens, the two Contacts2 (page 12)
/// elements that appear on Contacts items (ManagerName, CompanyMainPhone),
/// and the AirSyncBase (page 17) Body the 12.0+ wire uses ([MS-ASWBXML]
/// §2.1.2.1.2 note 1).
///
/// Malformed values → `log::warn!` (element name + offending text) then the
/// field's default; unmodeled tokens → `log::debug!` skip. Never panics.
///
/// Unmodeled BY DESIGN: the canonical skip list lives in the `tokens`
/// submodule header (M8-C task 2 decided those elements carry no value the
/// v1 contact model can use); the `_` arm below debug-skips them, pinned
/// by `exotic_contact_elements_are_skipped` in tests/commands_contacts.rs.
///
/// The `Err` arm exists for API symmetry with the sync parsers (which return
/// `Result<_, WbxmlError>`); today every malformed shape degrades to a
/// warning + default, so this always returns `Ok`.
///
/// # Errors
///
/// Does not error: every element is either mapped or warn/debug-logged and
/// skipped (the permissive ApplicationData contract). The `Result` keeps the
/// parse-family signature so the Sync dispatcher stays uniform.
pub fn parse_contacts_application_data(
    app_data: &WbxmlElement,
) -> Result<ContactsContactProps, WbxmlError> {
    let mut props = ContactsContactProps::default();
    for child in &app_data.children {
        match (child.page, child.token) {
            (PAGE_CONTACTS, CON_FILE_AS) => props.file_as = text_value_opt(child),
            (PAGE_CONTACTS, CON_FIRST_NAME) => props.first_name = text_value_opt(child),
            (PAGE_CONTACTS, CON_MIDDLE_NAME) => props.middle_name = text_value_opt(child),
            (PAGE_CONTACTS, CON_LAST_NAME) => props.last_name = text_value_opt(child),
            (PAGE_CONTACTS, CON_SUFFIX) => props.name_suffix = text_value_opt(child),
            (PAGE_CONTACTS, CON_TITLE) => props.name_prefix = text_value_opt(child),
            (PAGE_CONTACTS, CON_EMAIL_1) => {
                props.email_1 = parse_email_field("Email1Address", child);
            }
            (PAGE_CONTACTS, CON_EMAIL_2) => {
                props.email_2 = parse_email_field("Email2Address", child);
            }
            (PAGE_CONTACTS, CON_EMAIL_3) => {
                props.email_3 = parse_email_field("Email3Address", child);
            }
            (PAGE_CONTACTS, CON_COMPANY_NAME) => props.company = text_value_opt(child),
            (PAGE_CONTACTS, CON_JOB_TITLE) => props.job_title = text_value_opt(child),
            // 12.0+ contact bodies (the notes field) arrive as
            // airsyncbase:Body ([MS-ASWBXML] §2.1.2.1.2 note 1).
            (pages::BASE, base::BODY) => props.body_plain = parse_contacts_body(child),
            (PAGE_CONTACTS, CON_BUSINESS_PHONE) => {
                props.business_phone = text_value_opt(child);
            }
            (PAGE_CONTACTS, CON_HOME_PHONE) => props.home_phone = text_value_opt(child),
            (PAGE_CONTACTS, CON_MOBILE_PHONE) => props.mobile_phone = text_value_opt(child),
            // ---- M8-C task 2 ----
            // Dates stay raw wire strings — no parsing here.
            (PAGE_CONTACTS, CON_ANNIVERSARY) => props.anniversary = text_value_opt(child),
            (PAGE_CONTACTS, CON_BIRTHDAY) => props.birthday = text_value_opt(child),
            (PAGE_CONTACTS, CON_ASSISTANT_NAME) => props.assistant_name = text_value_opt(child),
            (pages::CONTACTS2, CON2_MANAGER_NAME) => {
                props.manager_name = text_value_opt(child);
            }
            // Remaining practical phones.
            (PAGE_CONTACTS, CON_ASSISTANT_PHONE) => {
                props.assistant_phone = text_value_opt(child);
            }
            (PAGE_CONTACTS, CON_BUSINESS_2_PHONE) => {
                props.business_2_phone = text_value_opt(child);
            }
            (PAGE_CONTACTS, CON_BUSINESS_FAX) => props.business_fax = text_value_opt(child),
            (PAGE_CONTACTS, CON_CAR_PHONE) => props.car_phone = text_value_opt(child),
            (pages::CONTACTS2, CON2_COMPANY_MAIN_PHONE) => {
                props.company_main_phone = text_value_opt(child);
            }
            (PAGE_CONTACTS, CON_HOME_2_PHONE) => props.home_2_phone = text_value_opt(child),
            (PAGE_CONTACTS, CON_HOME_FAX) => props.home_fax = text_value_opt(child),
            (PAGE_CONTACTS, CON_PAGER) => props.pager = text_value_opt(child),
            (PAGE_CONTACTS, CON_RADIO_PHONE) => props.radio_phone = text_value_opt(child),
            // The three address sets — five flat elements each
            // ([MS-ASCNTC] §2.2.2.10-.14 / .33-.37 / .52-.56).
            (PAGE_CONTACTS, CON_BUSINESS_ADDRESS_STREET) => {
                set_address_field(&mut props.business_address, child, |a| &mut a.street);
            }
            (PAGE_CONTACTS, CON_BUSINESS_ADDRESS_CITY) => {
                set_address_field(&mut props.business_address, child, |a| &mut a.city);
            }
            (PAGE_CONTACTS, CON_BUSINESS_ADDRESS_STATE) => {
                set_address_field(&mut props.business_address, child, |a| &mut a.state);
            }
            (PAGE_CONTACTS, CON_BUSINESS_ADDRESS_POSTAL_CODE) => {
                set_address_field(&mut props.business_address, child, |a| &mut a.postal_code);
            }
            (PAGE_CONTACTS, CON_BUSINESS_ADDRESS_COUNTRY) => {
                set_address_field(&mut props.business_address, child, |a| &mut a.country);
            }
            (PAGE_CONTACTS, CON_HOME_ADDRESS_STREET) => {
                set_address_field(&mut props.home_address, child, |a| &mut a.street);
            }
            (PAGE_CONTACTS, CON_HOME_ADDRESS_CITY) => {
                set_address_field(&mut props.home_address, child, |a| &mut a.city);
            }
            (PAGE_CONTACTS, CON_HOME_ADDRESS_STATE) => {
                set_address_field(&mut props.home_address, child, |a| &mut a.state);
            }
            (PAGE_CONTACTS, CON_HOME_ADDRESS_POSTAL_CODE) => {
                set_address_field(&mut props.home_address, child, |a| &mut a.postal_code);
            }
            (PAGE_CONTACTS, CON_HOME_ADDRESS_COUNTRY) => {
                set_address_field(&mut props.home_address, child, |a| &mut a.country);
            }
            (PAGE_CONTACTS, CON_OTHER_ADDRESS_STREET) => {
                set_address_field(&mut props.other_address, child, |a| &mut a.street);
            }
            (PAGE_CONTACTS, CON_OTHER_ADDRESS_CITY) => {
                set_address_field(&mut props.other_address, child, |a| &mut a.city);
            }
            (PAGE_CONTACTS, CON_OTHER_ADDRESS_STATE) => {
                set_address_field(&mut props.other_address, child, |a| &mut a.state);
            }
            (PAGE_CONTACTS, CON_OTHER_ADDRESS_POSTAL_CODE) => {
                set_address_field(&mut props.other_address, child, |a| &mut a.postal_code);
            }
            (PAGE_CONTACTS, CON_OTHER_ADDRESS_COUNTRY) => {
                set_address_field(&mut props.other_address, child, |a| &mut a.country);
            }
            (PAGE_CONTACTS, CON_WEB_PAGE) => props.web_page = text_value_opt(child),
            // Presence only — the base64 payload is dropped here.
            (PAGE_CONTACTS, CON_PICTURE) => props.picture_present = parse_picture_present(child),
            _ => {
                // Known-but-exotic contact elements (canonical skip list
                // in the `tokens` submodule) plus unknown garbage.
                log::debug!(
                    "contacts ApplicationData: skipping unmodeled element {} \
                     (page {} token 0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    Ok(props)
}

// ============================================================================
// Field-parse helpers (permissive: warn + default, never panic — the
// `calendar/` / `commands::sync` ApplicationData precedent)
// ============================================================================

/// Permissive text extraction — the `calendar::text_value_opt` twin:
/// missing or non-text values map to `None` rather than aborting the item
/// parse. (Local copy because the calendar-module helper is private.)
fn text_value_opt(elem: &WbxmlElement) -> Option<String> {
    match &elem.value {
        WbxmlValue::Text(s) => Some(s.clone()),
        WbxmlValue::Opaque(b) => std::str::from_utf8(b)
            .ok()
            .map(std::string::ToString::to_string),
        WbxmlValue::Empty => None,
    }
}

/// E-mail address field: strip display-name formatting
/// ([`extract_bare_address`]), then keep the result when it has the
/// minimal SMTP shape (an `@` and no residual brackets — [MS-ASCNTC]
/// §2.2.2.27 calls for an e-mail address; this is a wire-shape sanity
/// check, not a full RFC 5322 parse). Text failing the shape, or an
/// element without a text value where text is expected, warns and
/// degrades to `None` (loud, never silent, never panic).
fn parse_email_field(name: &'static str, elem: &WbxmlElement) -> Option<String> {
    if let Some(raw) = text_value_opt(elem) {
        let bare = extract_bare_address(&raw);
        if bare.contains('@') && !bare.contains('<') && !bare.contains('>') {
            Some(bare)
        } else {
            log::warn!(
                "contacts ApplicationData: malformed {name} \"{raw}\"; expected an \
                 e-mail address, ignoring"
            );
            None
        }
    } else {
        log::warn!(
            "contacts ApplicationData: {name} element without a text value; \
             treating it as absent"
        );
        None
    }
}

/// Extract the bare SMTP address from a display-formatted EmailAddress
/// value. [MS-ASCNTC] §2.2.2.27-§2.2.2.29 say the elements carry "an
/// e-mail address", but Exchange/OWA frequently serialize the full
/// RFC 5322 mailbox form instead — `"Display Name" <local@domain>`
/// (observed live in the 2026-08-17 seed drill: the whole quoted string
/// landed in `email_1`, breaking the contacts.email UNIQUE dedup,
/// composer autocomplete, and vCard EMAIL export).
///
/// Ruling (M8-C1, documented here): the embedded display name is
/// DROPPED, with a debug log — the contact's display name comes from
/// FileAs ([MS-ASCNTC] §2.2.2.30), and no `email_display_*` field is
/// added to the struct. This helper NEVER invents an address: when the
/// angle-bracket content is not an address (or a bracket is unclosed),
/// the original value is returned UNCHANGED with a warn, leaving the
/// caller's SMTP-shape check to reject it.
pub(super) fn extract_bare_address(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(open) = trimmed.find('<') else {
        return raw.to_string(); // plain form: nothing to strip
    };
    let Some(close) = trimmed[open + 1..].find('>') else {
        log::warn!(
            "contacts ApplicationData: e-mail value \"{raw}\" has an unclosed '<'; \
             keeping it as-is for the shape check"
        );
        return raw.to_string();
    };
    let inner = trimmed[open + 1..open + 1 + close].trim();
    if !inner.contains('@') {
        log::warn!(
            "contacts ApplicationData: angle-bracketed e-mail value \"{raw}\" does not \
             contain an address; keeping it as-is (never inventing one)"
        );
        return raw.to_string();
    }
    let display = trimmed[..open].trim().trim_matches('"').trim();
    if !display.is_empty() {
        log::debug!(
            "contacts ApplicationData: dropping embedded display name {display:?} from \
             an e-mail value (the contact's display name comes from FileAs)"
        );
    }
    inner.to_string()
}

/// `airsyncbase:Body` on a contact item → the plain-text notes payload, if
/// any. Type 1 (PlainText) fills `body_plain`; Type 2 (HTML) / Type 4
/// (MIME) are valid wire data but not modeled on contact items in v1
/// (debug-logged); a Body without a parseable Type warns and keeps the data
/// as plain (graceful degradation, the `calendar::parse_calendar_body`
/// precedent).
fn parse_contacts_body(elem: &WbxmlElement) -> Option<String> {
    let mut body_type: Option<u8> = None;
    let mut data: Option<String> = None;
    for child in &elem.children {
        match child.tag_name() {
            "Type" => body_type = text_value_opt(child).and_then(|s| s.parse().ok()),
            "Data" => data = text_value_opt(child),
            "EstimatedDataSize" | "Truncated" => {} // not surfaced on contact items
            _ => {
                log::debug!(
                    "contacts ApplicationData: skipping unexpected Body child {} \
                     (page {} token 0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    match body_type {
        Some(1) => data,
        Some(other) => {
            log::debug!(
                "contacts ApplicationData: Body Type {other} (not PlainText) — \
                 contact bodies are plain-only in v1; skipping payload"
            );
            None
        }
        None => {
            if data.is_some() {
                log::warn!(
                    "contacts ApplicationData: Body without a parseable Type; \
                     keeping payload as plain text"
                );
            }
            data
        }
    }
}

/// Set one component of an address set, opening the set on first sight.
/// An element with an empty/ghosted value still opens the set (the element
/// appeared on the wire) but leaves the component `None` — the cleared
/// shape the conversion layer needs to see.
fn set_address_field(
    slot: &mut Option<ContactsAddress>,
    elem: &WbxmlElement,
    field: impl FnOnce(&mut ContactsAddress) -> &mut Option<String>,
) {
    *field(slot.get_or_insert_with(ContactsAddress::default)) = text_value_opt(elem);
}

/// `Picture` ([MS-ASCNTC] §2.2.2.58) is PRESENCE-only in v1: a Picture
/// element with a non-empty value sets `picture_present`, and the base64
/// payload is dropped right here — only its byte length reaches the log
/// (the payload itself must never be logged: up to 48 KB of base64).
/// Empty-valued elements (cleared/ghosted pictures) count as absent.
/// Pinned by the picture tests in tests/commands_contacts.rs.
fn parse_picture_present(elem: &WbxmlElement) -> bool {
    match &elem.value {
        WbxmlValue::Text(s) if !s.is_empty() => {
            log::debug!(
                "contacts ApplicationData: Picture present — dropping {}-byte \
                 payload (presence-only in v1)",
                s.len()
            );
            true
        }
        WbxmlValue::Opaque(b) if !b.is_empty() => {
            log::debug!(
                "contacts ApplicationData: Picture present — dropping {}-byte \
                 opaque payload (presence-only in v1)",
                b.len()
            );
            true
        }
        WbxmlValue::Text(_) | WbxmlValue::Opaque(_) => {
            log::warn!(
                "contacts ApplicationData: Picture with an empty value; \
                 treating the contact picture as absent"
            );
            false
        }
        WbxmlValue::Empty => {
            log::warn!(
                "contacts ApplicationData: Picture element without a value; \
                 treating the contact picture as absent"
            );
            false
        }
    }
}

/// Human-readable tag name for log lines — unlike `WbxmlElement::tag_name()`
/// this never warns on unregistered tokens (a skip line must not spawn a
/// second warn for the same element). Twin of `calendar::tag_label`.
fn tag_label(elem: &WbxmlElement) -> String {
    match crate::wbxml::code_page(elem.page).and_then(|p| p.tag_name(elem.token)) {
        Some(name) => name.to_string(),
        None => format!("unknown-0x{:02X}", elem.token),
    }
}
