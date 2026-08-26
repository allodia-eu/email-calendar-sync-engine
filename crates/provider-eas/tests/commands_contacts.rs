// SPDX-License-Identifier: MPL-2.0
//! M8-C task 2 (C2): MS-ASCNTC Contacts-class ApplicationData — per-field
//! tests for the extended field set (address sets, remaining practical
//! phones, anniversary/birthday raw dates, assistant/manager, web page,
//! picture presence).
//!
//! The golden full-contact fixture + whole-struct equality test live in
//! `src/contacts_testutil.rs` (`#[cfg(test)] pub(crate)`, shared with the
//! Sync seam tests in `src/commands/sync/tests.rs` — one source of truth). This file
//! builds small, focused wire trees through the pub parser API.
//!
//! Token fidelity: every (page, token) pair used below is verified against
//! `docs/Exchange/MS-ASWBXML.txt` §2.1.2.1.2 ("Code Page 1: Contacts") and
//! §2.1.2.1.13 ("Code Page 12: Contacts2"), v20220429 — pinned by
//! `contacts_token_constants_match_spec` below, never from memory.

use provider_eas::{
    commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    contacts::{
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
        CON_WEB_PAGE, CON2_COMPANY_MAIN_PHONE, CON2_MANAGER_NAME, ContactsAddress,
        ContactsContactProps, PAGE_CONTACTS, parse_contacts_application_data,
    },
    wbxml::{
        WbxmlElement,
        tags::{base, pages},
    },
};

#[path = "commands_contacts/parse_fields.rs"]
mod parse_fields;
#[path = "commands_contacts/token_constants.rs"]
mod token_constants;

/// Wrap children in an `airsync:ApplicationData` container, mirroring the
/// shape of a Sync Add/Change item payload.
fn app_data(children: Vec<WbxmlElement>) -> WbxmlElement {
    WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, children)
}

/// One page-1 Contacts text element.
fn con(token: u8, text: &str) -> WbxmlElement {
    WbxmlElement::text(PAGE_CONTACTS, token, text)
}
