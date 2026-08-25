// SPDX-License-Identifier: MPL-2.0
//! Contacts token fidelity: every (page, token) constant cross-checked against the spec.

use super::*;

// ============================================================================
// Token fidelity (moved here from the contacts.rs inline module, M8-C
// task 2 fix r1 — all constants and `tag_name()` are pub API)
// ============================================================================

/// Cross-check every constant against its [MS-ASWBXML] §2.1.2.1.2 value
/// AND against the `code_pages.rs` registration (tag_name resolution),
/// so a drifted constant fails loudly.
#[test]
fn contacts_token_constants_match_spec() {
    assert_eq!(PAGE_CONTACTS, 1);
    assert_eq!(CON_FILE_AS, 0x1E);
    assert_eq!(CON_FIRST_NAME, 0x1F);
    assert_eq!(CON_MIDDLE_NAME, 0x2A);
    assert_eq!(CON_LAST_NAME, 0x29);
    assert_eq!(CON_SUFFIX, 0x35);
    assert_eq!(CON_TITLE, 0x36);
    assert_eq!(CON_EMAIL_1, 0x1B);
    assert_eq!(CON_EMAIL_2, 0x1C);
    assert_eq!(CON_EMAIL_3, 0x1D);
    assert_eq!(CON_COMPANY_NAME, 0x19);
    assert_eq!(CON_JOB_TITLE, 0x28);
    assert_eq!(CON_BUSINESS_PHONE, 0x13);
    assert_eq!(CON_HOME_PHONE, 0x27);
    assert_eq!(CON_MOBILE_PHONE, 0x2B);
    // M8-C task 2 set.
    assert_eq!(CON_ANNIVERSARY, 0x05);
    assert_eq!(CON_ASSISTANT_NAME, 0x06);
    assert_eq!(CON_ASSISTANT_PHONE, 0x07);
    assert_eq!(CON_BIRTHDAY, 0x08);
    assert_eq!(CON_BUSINESS_2_PHONE, 0x0C);
    assert_eq!(CON_BUSINESS_ADDRESS_CITY, 0x0D);
    assert_eq!(CON_BUSINESS_ADDRESS_COUNTRY, 0x0E);
    assert_eq!(CON_BUSINESS_ADDRESS_POSTAL_CODE, 0x0F);
    assert_eq!(CON_BUSINESS_ADDRESS_STATE, 0x10);
    assert_eq!(CON_BUSINESS_ADDRESS_STREET, 0x11);
    assert_eq!(CON_BUSINESS_FAX, 0x12);
    assert_eq!(CON_CAR_PHONE, 0x14);
    assert_eq!(CON_HOME_2_PHONE, 0x20);
    assert_eq!(CON_HOME_ADDRESS_CITY, 0x21);
    assert_eq!(CON_HOME_ADDRESS_COUNTRY, 0x22);
    assert_eq!(CON_HOME_ADDRESS_POSTAL_CODE, 0x23);
    assert_eq!(CON_HOME_ADDRESS_STATE, 0x24);
    assert_eq!(CON_HOME_ADDRESS_STREET, 0x25);
    assert_eq!(CON_HOME_FAX, 0x26);
    assert_eq!(CON_OTHER_ADDRESS_CITY, 0x2D);
    assert_eq!(CON_OTHER_ADDRESS_COUNTRY, 0x2E);
    assert_eq!(CON_OTHER_ADDRESS_POSTAL_CODE, 0x2F);
    assert_eq!(CON_OTHER_ADDRESS_STATE, 0x30);
    assert_eq!(CON_OTHER_ADDRESS_STREET, 0x31);
    assert_eq!(CON_PAGER, 0x32);
    assert_eq!(CON_RADIO_PHONE, 0x33);
    assert_eq!(CON_WEB_PAGE, 0x37);
    assert_eq!(CON_PICTURE, 0x3C);
    // Contacts2 (page 12, [MS-ASWBXML] §2.1.2.1.13).
    assert_eq!(CON2_MANAGER_NAME, 0x0A);
    assert_eq!(CON2_COMPANY_MAIN_PHONE, 0x0B);

    // tag_name() resolution — cross-checks code_pages.rs CONTACTS_TOKENS
    // (and CONTACTS2_TOKENS for the page-12 pair).
    let cases: &[(u8, u8, &str)] = &[
        (PAGE_CONTACTS, CON_FILE_AS, "FileAs"),
        (PAGE_CONTACTS, CON_FIRST_NAME, "FirstName"),
        (PAGE_CONTACTS, CON_MIDDLE_NAME, "MiddleName"),
        (PAGE_CONTACTS, CON_LAST_NAME, "LastName"),
        (PAGE_CONTACTS, CON_SUFFIX, "Suffix"),
        (PAGE_CONTACTS, CON_TITLE, "Title"),
        (PAGE_CONTACTS, CON_EMAIL_1, "Email1Address"),
        (PAGE_CONTACTS, CON_EMAIL_2, "Email2Address"),
        (PAGE_CONTACTS, CON_EMAIL_3, "Email3Address"),
        (PAGE_CONTACTS, CON_COMPANY_NAME, "CompanyName"),
        (PAGE_CONTACTS, CON_JOB_TITLE, "JobTitle"),
        (PAGE_CONTACTS, CON_BUSINESS_PHONE, "BusinessPhoneNumber"),
        (PAGE_CONTACTS, CON_HOME_PHONE, "HomePhoneNumber"),
        (PAGE_CONTACTS, CON_MOBILE_PHONE, "MobilePhoneNumber"),
        // M8-C task 2 set.
        (PAGE_CONTACTS, CON_ANNIVERSARY, "Anniversary"),
        (PAGE_CONTACTS, CON_ASSISTANT_NAME, "AssistantName"),
        (PAGE_CONTACTS, CON_ASSISTANT_PHONE, "AssistantPhoneNumber"),
        (PAGE_CONTACTS, CON_BIRTHDAY, "Birthday"),
        (PAGE_CONTACTS, CON_BUSINESS_2_PHONE, "Business2PhoneNumber"),
        (
            PAGE_CONTACTS,
            CON_BUSINESS_ADDRESS_CITY,
            "BusinessAddressCity",
        ),
        (
            PAGE_CONTACTS,
            CON_BUSINESS_ADDRESS_COUNTRY,
            "BusinessAddressCountry",
        ),
        (
            PAGE_CONTACTS,
            CON_BUSINESS_ADDRESS_POSTAL_CODE,
            "BusinessAddressPostalCode",
        ),
        (
            PAGE_CONTACTS,
            CON_BUSINESS_ADDRESS_STATE,
            "BusinessAddressState",
        ),
        (
            PAGE_CONTACTS,
            CON_BUSINESS_ADDRESS_STREET,
            "BusinessAddressStreet",
        ),
        (PAGE_CONTACTS, CON_BUSINESS_FAX, "BusinessFaxNumber"),
        (PAGE_CONTACTS, CON_CAR_PHONE, "CarPhoneNumber"),
        (PAGE_CONTACTS, CON_HOME_2_PHONE, "Home2PhoneNumber"),
        (PAGE_CONTACTS, CON_HOME_ADDRESS_CITY, "HomeAddressCity"),
        (
            PAGE_CONTACTS,
            CON_HOME_ADDRESS_COUNTRY,
            "HomeAddressCountry",
        ),
        (
            PAGE_CONTACTS,
            CON_HOME_ADDRESS_POSTAL_CODE,
            "HomeAddressPostalCode",
        ),
        (PAGE_CONTACTS, CON_HOME_ADDRESS_STATE, "HomeAddressState"),
        (PAGE_CONTACTS, CON_HOME_ADDRESS_STREET, "HomeAddressStreet"),
        (PAGE_CONTACTS, CON_HOME_FAX, "HomeFaxNumber"),
        (PAGE_CONTACTS, CON_OTHER_ADDRESS_CITY, "OtherAddressCity"),
        (
            PAGE_CONTACTS,
            CON_OTHER_ADDRESS_COUNTRY,
            "OtherAddressCountry",
        ),
        (
            PAGE_CONTACTS,
            CON_OTHER_ADDRESS_POSTAL_CODE,
            "OtherAddressPostalCode",
        ),
        (PAGE_CONTACTS, CON_OTHER_ADDRESS_STATE, "OtherAddressState"),
        (
            PAGE_CONTACTS,
            CON_OTHER_ADDRESS_STREET,
            "OtherAddressStreet",
        ),
        (PAGE_CONTACTS, CON_PAGER, "PagerNumber"),
        (PAGE_CONTACTS, CON_RADIO_PHONE, "RadioPhoneNumber"),
        (PAGE_CONTACTS, CON_WEB_PAGE, "WebPage"),
        (PAGE_CONTACTS, CON_PICTURE, "Picture"),
        (pages::CONTACTS2, CON2_MANAGER_NAME, "ManagerName"),
        (
            pages::CONTACTS2,
            CON2_COMPANY_MAIN_PHONE,
            "CompanyMainPhone",
        ),
    ];
    for &(page, token, name) in cases {
        assert_eq!(
            WbxmlElement::empty(page, token).tag_name(),
            name,
            "({page}, 0x{token:02X}) must resolve to {name}"
        );
    }
    // Body children used by the parser (page 17, §2.1.2.1.18).
    assert_eq!(base::BODY, 0x0A);
    assert_eq!(base::TYPE, 0x06);
    assert_eq!(base::DATA, 0x0B);
}
