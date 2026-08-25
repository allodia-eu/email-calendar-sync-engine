// SPDX-License-Identifier: MPL-2.0
//! MS-ASCNTC Contacts-class item model (the Contact Class protocol the
//! M8-C plan calls MS-ASAIRCONT; `docs/Exchange/[MS-ASCNTC].pdf`) +
//! downsync parse of a Contacts-class `ApplicationData` element.
//!
//! Token fidelity red line: every token value lives in the `tokens`
//! submodule (split out for the 500-line rule, re-exported below so
//! `contacts::CON_*` paths stay stable) with its [MS-ASWBXML] /
//! [MS-ASCNTC] citations — never from memory.
//!
//! Downsync only: v1 never BUILDS Contacts-class items for upload.
//!
//! Parse policy (the Email `parse_application_data` precedent in
//! `commands/sync.rs` and the `calendar.rs` twin): malformed values →
//! `log::warn!` with the element name + offending text, then default —
//! never panic, never swallow silently; tokens this task does not model →
//! `log::debug!` skip.

use serde::{Deserialize, Serialize};

use crate::wbxml::{
    WbxmlElement, WbxmlError, WbxmlValue,
    tags::{base, pages},
};

// ============================================================================
// Code-page tag constants — the `tokens` submodule (M8-C task 2 fix r1:
// split out of this file for the 500-line rule). Citations travel with
// the constants; the canonical unmodeled-skip list lives there too.
// Re-exported so every existing `contacts::CON_*` path stays stable.
// ============================================================================

mod tokens;
pub use tokens::*;

// ============================================================================
// Model types ([MS-ASCNTC] §2.2)
// ============================================================================

/// One address set of a contact — the Business / Home / Other triples of
/// [MS-ASCNTC]. The wire carries each component as its own flat element
/// (BusinessAddressStreet, BusinessAddressCity, … — every component "can
/// be ghosted", §2.2.2.10 et al.); grouping the five components per set
/// mirrors the Outlook object model and hands the sync/store conversion
/// layer one natural unit per address. A set that appeared on the wire
/// with only some components is `Some(ContactsAddress { ..partial })`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactsAddress {
    /// `...AddressStreet` ([MS-ASCNTC] §2.2.2.14 / §2.2.2.37 / §2.2.2.56).
    pub street: Option<String>,
    /// `...AddressCity` ([MS-ASCNTC] §2.2.2.10 / §2.2.2.33 / §2.2.2.52).
    pub city: Option<String>,
    /// `...AddressState` ([MS-ASCNTC] §2.2.2.13 / §2.2.2.36 / §2.2.2.55).
    pub state: Option<String>,
    /// `...AddressPostalCode` ([MS-ASCNTC] §2.2.2.12 / §2.2.2.35 /
    /// §2.2.2.54).
    pub postal_code: Option<String>,
    /// `...AddressCountry` ([MS-ASCNTC] §2.2.2.11 / §2.2.2.34 /
    /// §2.2.2.53).
    pub country: Option<String>,
}

/// One EAS Contacts item's application data (downsync model; v1 never builds
/// these for upload). Raw wire values; conversion to the app's
/// `ParsedContact` happens in the backend (sync/store layer), not here.
///
/// C1 core set: name parts + filing, the three e-mail slots, company/job,
/// the plain-text body (contact notes), and the three primary phone slots.
/// M8-C task 2 added: the three address sets, the remaining practical
/// phones, anniversary/birthday (raw wire strings), assistant/manager,
/// web page, and picture PRESENCE (the Picture payload itself is dropped
/// at parse time — see `parse_picture_present`).
///
/// Serde derives: the type rides inside `SyncResult`
/// (`ContactsItemWithId`), which itself derives `Serialize`/`Deserialize`
/// — mirrors the `CalendarEventProps` precedent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactsContactProps {
    /// `FileAs` — how the contact is filed ([MS-ASCNTC] §2.2.2.30).
    pub file_as: Option<String>,
    /// `FirstName` ([MS-ASCNTC] §2.2.2.31).
    pub first_name: Option<String>,
    /// `MiddleName` ([MS-ASCNTC] §2.2.2.47).
    pub middle_name: Option<String>,
    /// `LastName` ([MS-ASCNTC] §2.2.2.45).
    pub last_name: Option<String>,
    /// `Suffix` — name suffix, e.g. "Jr." ([MS-ASCNTC] §2.2.2.61).
    pub name_suffix: Option<String>,
    /// `Title` — name prefix, e.g. "Mr." ([MS-ASCNTC] §2.2.2.62).
    pub name_prefix: Option<String>,
    /// `Email1Address` — the primary e-mail address
    /// ([MS-ASCNTC] §2.2.2.27).
    pub email_1: Option<String>,
    /// `Email2Address` ([MS-ASCNTC] §2.2.2.28).
    pub email_2: Option<String>,
    /// `Email3Address` ([MS-ASCNTC] §2.2.2.29).
    pub email_3: Option<String>,
    /// `CompanyName` ([MS-ASCNTC] §2.2.2.24).
    pub company: Option<String>,
    /// `JobTitle` ([MS-ASCNTC] §2.2.2.44).
    pub job_title: Option<String>,
    /// Plain-text body: `airsyncbase:Body` with `Type = 1` (PlainText) —
    /// the contact's notes ([MS-ASCNTC] §2.2.2.7.1; the 2.5-only
    /// contacts-page Body is not modeled). HTML/MIME bodies are not
    /// modeled on contact items in v1.
    pub body_plain: Option<String>,
    /// `BusinessPhoneNumber` — the primary business line
    /// ([MS-ASCNTC] §2.2.2.16).
    pub business_phone: Option<String>,
    /// `HomePhoneNumber` ([MS-ASCNTC] §2.2.2.39).
    pub home_phone: Option<String>,
    /// `MobilePhoneNumber` ([MS-ASCNTC] §2.2.2.49).
    pub mobile_phone: Option<String>,

    // ---- M8-C task 2 ----
    /// `Anniversary` — wedding anniversary ([MS-ASCNTC] §2.2.2.3). The
    /// wire value verbatim: dateTime per [MS-ASDTYPE] §2.3
    /// (`YYYY-MM-DDTHH:MM:SS.MSSZ`, UTC; the time part "might be 11:59
    /// and SHOULD be ignored") — RAW string, no date parsing in the
    /// parser; interpretation belongs to the conversion layer.
    pub anniversary: Option<String>,
    /// `Birthday` — birth date ([MS-ASCNTC] §2.2.2.6). Raw wire string,
    /// same contract as [`Self::anniversary`].
    pub birthday: Option<String>,
    /// `AssistantName` ([MS-ASCNTC] §2.2.2.4).
    pub assistant_name: Option<String>,
    /// `contacts2:ManagerName` (page 12) — the DN of the contact's
    /// manager ([MS-ASCNTC] §2.2.2.46).
    pub manager_name: Option<String>,
    /// `AssistantPhoneNumber` ([MS-ASCNTC] §2.2.2.5).
    pub assistant_phone: Option<String>,
    /// `Business2PhoneNumber` — the second business line
    /// ([MS-ASCNTC] §2.2.2.17).
    pub business_2_phone: Option<String>,
    /// `BusinessFaxNumber` ([MS-ASCNTC] §2.2.2.15).
    pub business_fax: Option<String>,
    /// `CarPhoneNumber` ([MS-ASCNTC] §2.2.2.18).
    pub car_phone: Option<String>,
    /// `contacts2:CompanyMainPhone` (page 12) — the company's main line
    /// ([MS-ASCNTC] §2.2.2.23).
    pub company_main_phone: Option<String>,
    /// `Home2PhoneNumber` — the second home line
    /// ([MS-ASCNTC] §2.2.2.40).
    pub home_2_phone: Option<String>,
    /// `HomeFaxNumber` ([MS-ASCNTC] §2.2.2.38).
    pub home_fax: Option<String>,
    /// `PagerNumber` ([MS-ASCNTC] §2.2.2.57).
    pub pager: Option<String>,
    /// `RadioPhoneNumber` ([MS-ASCNTC] §2.2.2.59).
    pub radio_phone: Option<String>,
    /// Business address set — the five `BusinessAddress*` elements
    /// ([MS-ASCNTC] §2.2.2.10-.14); `None` when none appeared on the wire.
    pub business_address: Option<ContactsAddress>,
    /// Home address set — the five `HomeAddress*` elements
    /// ([MS-ASCNTC] §2.2.2.33-.37).
    pub home_address: Option<ContactsAddress>,
    /// Other (alternate) address set — the five `OtherAddress*` elements
    /// ([MS-ASCNTC] §2.2.2.52-.56).
    pub other_address: Option<ContactsAddress>,
    /// `WebPage` — the contact's web site / personal page
    /// ([MS-ASCNTC] §2.2.2.63).
    pub web_page: Option<String>,
    /// `Picture` PRESENCE only ([MS-ASCNTC] §2.2.2.58). v1 never retains
    /// the base64 payload — it is dropped at parse time with a
    /// `log::debug!` (pinned by the picture-drop tests); `true` iff a
    /// Picture element with a non-empty value appeared on the wire.
    pub picture_present: bool,
}

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
// calendar.rs / commands::sync ApplicationData precedent)
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
fn extract_bare_address(raw: &str) -> String {
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

// ============================================================================
// Tests
// ============================================================================

// `pub(crate)` (M8-C task 1): visibility only. M8-C task 2 fix r1 moved
// the golden fixture pair + the golden whole-struct test to
// `crate::contacts_testutil` (shared with the Sync seam tests in
// `commands/sync.rs`) and the token-spec test to
// tests/commands_contacts.rs; the per-behavior parser tests stay here.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC};

    /// Minimal item: FileAs only — the filing name every server-side
    /// contact carries; everything else stays `None`.
    #[test]
    fn parse_minimal_file_as_only_item() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(
                PAGE_CONTACTS,
                CON_FILE_AS,
                "Kerry, Anat",
            )],
        );
        let props = parse_contacts_application_data(&app_data).expect("parse ok");
        assert_eq!(props.file_as.as_deref(), Some("Kerry, Anat"));
        assert_eq!(props.first_name, None);
        assert_eq!(props.middle_name, None);
        assert_eq!(props.last_name, None);
        assert_eq!(props.name_suffix, None);
        assert_eq!(props.name_prefix, None);
        assert_eq!(props.email_1, None);
        assert_eq!(props.email_2, None);
        assert_eq!(props.email_3, None);
        assert_eq!(props.company, None);
        assert_eq!(props.job_title, None);
        assert_eq!(props.body_plain, None);
        assert_eq!(props.business_phone, None);
        assert_eq!(props.home_phone, None);
        assert_eq!(props.mobile_phone, None);
    }

    /// Absent optionals: an empty ApplicationData yields all defaults —
    /// no panic, no phantom Some values.
    #[test]
    fn parse_absent_optionals_are_defaults() {
        let app_data = WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, vec![]);
        let props = parse_contacts_application_data(&app_data).expect("parse ok");
        assert_eq!(props, ContactsContactProps::default());
    }

    /// Malformed e-mail values degrade to None with a warning — never
    /// panic, and sibling fields still parse (a bad e-mail must not poison
    /// the rest of the item). Two wire shapes: text that is not an SMTP
    /// address (no `@`), and an element without a text value where text is
    /// expected.
    #[test]
    fn parse_malformed_email_warns_and_defaults_none() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CONTACTS, CON_FILE_AS, "Bad Mail"),
                WbxmlElement::text(PAGE_CONTACTS, CON_EMAIL_1, "not-an-email"),
                WbxmlElement::empty(PAGE_CONTACTS, CON_EMAIL_2),
                WbxmlElement::text(PAGE_CONTACTS, CON_EMAIL_3, "ok@example.com"),
            ],
        );
        let props = parse_contacts_application_data(&app_data).expect("parse ok");
        assert_eq!(props.email_1, None, "non-SMTP Email1Address must be None");
        assert_eq!(props.email_2, None, "text-less Email2Address must be None");
        assert_eq!(props.email_3.as_deref(), Some("ok@example.com"));
        assert_eq!(props.file_as.as_deref(), Some("Bad Mail"));
    }

    /// M8-C1 helper matrix: bare-address extraction from the
    /// display-formatted e-mail values Exchange/OWA emits. The
    /// end-to-end matrix (through `parse_contacts_application_data`)
    /// lives in `tests/commands_contacts.rs`; this inline test is the
    /// only place the "kept as-is" rows are observable (the parse-level
    /// gate then rejects them).
    #[test]
    fn extract_bare_address_matrix() {
        // Quoted display name — the live OWA shape from the seed drill.
        assert_eq!(
            extract_bare_address("\"fileas\" <seed.contact@kylins.local>"),
            "seed.contact@kylins.local"
        );
        // Plain address: unchanged, untouched.
        assert_eq!(extract_bare_address("plain@x.y"), "plain@x.y");
        // Unquoted display name.
        assert_eq!(extract_bare_address("Name <a@b.c>"), "a@b.c");
        // Malformed: bracketed non-address and unclosed bracket are kept
        // AS-IS (never mangled, never invented).
        assert_eq!(extract_bare_address("<no-at>"), "<no-at>");
        assert_eq!(extract_bare_address("Name <a@b.c"), "Name <a@b.c");
        // Whitespace around the whole value is tolerated.
        assert_eq!(extract_bare_address("  \"N\"  <x@y.z>  "), "x@y.z");
        // An empty display name strips silently (nothing to log).
        assert_eq!(extract_bare_address("\"\" <e@f.g>"), "e@f.g");
    }

    /// Only a PlainText (Type 1) body fills `body_plain`; an HTML body is
    /// valid wire data but not modeled on contact items in v1.
    #[test]
    fn parse_body_plain_only_for_type_1() {
        let html = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "2"),
                    WbxmlElement::text(pages::BASE, base::DATA, "<p>html notes</p>"),
                ],
            )],
        );
        let props = parse_contacts_application_data(&html).expect("parse ok");
        assert_eq!(props.body_plain, None, "HTML contact body is not plain");
    }

    /// Unmodeled tokens (Department 0x1A, Categories container 0x15/0x16,
    /// Contacts2 NickName (page 12, 0x0D), unknown page/token garbage)
    /// are skipped without panic and do not disturb the modeled fields.
    /// (M8-C task 2: ManagerName graduated from this list into the modeled
    /// set — NickName takes its place as the unmodeled Contacts2 sample.)
    #[test]
    fn parse_unmodeled_tokens_are_skipped() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![
                WbxmlElement::text(PAGE_CONTACTS, 0x1A, "Engineering"), // Department
                WbxmlElement::container(
                    PAGE_CONTACTS,
                    0x15, // Categories
                    vec![WbxmlElement::text(PAGE_CONTACTS, 0x16, "VIP")],
                ),
                WbxmlElement::text(pages::CONTACTS2, 0x0D, "Bobby"), // NickName
                WbxmlElement::text(0xFE, 0x7F, "garbage"),
                WbxmlElement::text(PAGE_CONTACTS, CON_FILE_AS, "Real FileAs"),
            ],
        );
        let props = parse_contacts_application_data(&app_data).expect("parse ok");
        assert_eq!(props.file_as.as_deref(), Some("Real FileAs"));
        assert_eq!(props.company, None);
        assert_eq!(props.email_1, None);
    }
}
