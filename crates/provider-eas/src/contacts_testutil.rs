// SPDX-License-Identifier: MPL-2.0
//! Shared golden fixture for the Contacts-class parser (M8-C task 2 fix r1:
//! extracted from `contacts.rs`'s inline test module for the 500-line rule;
//! the code is byte-for-byte the C1/task-2 fixture, only the home moved).
//!
//! `#[cfg(test)] pub(crate)`: consumed by the class-aware Sync seam tests
//! in `commands/sync.rs` and by the golden test below — one source of
//! truth for the golden wire shape, no duplication.

use crate::{
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

/// Fixture: a fully-populated Contacts ApplicationData covering every
/// C1 core field AND every M8-C task 2 field — one DISTINCT value per
/// field so a crossed wire cannot hide behind equal props. Token
/// layout (page, token):
/// ```text
/// ApplicationData (0, 0x1D)
///   ├── FileAs            (1, 0x1E) = "Zhou, Felix"
///   ├── FirstName         (1, 0x1F) = "Felix"
///   ├── MiddleName        (1, 0x2A) = "Ming"
///   ├── LastName          (1, 0x29) = "Zhou"
///   ├── Suffix            (1, 0x35) = "Jr."
///   ├── Title             (1, 0x36) = "Mr."
///   ├── Email1Address     (1, 0x1B) = "felixzhou@kylins.local"
///   ├── Email2Address     (1, 0x1C) = "felix.zhou@example.com"
///   ├── Email3Address     (1, 0x1D) = "felix@home.example"
///   ├── CompanyName       (1, 0x19) = "Kylins"
///   ├── JobTitle          (1, 0x28) = "Development Manager"
///   ├── BusinessPhoneNumber (1, 0x13) = "(206) 555-0103"
///   ├── HomePhoneNumber   (1, 0x27) = "(206) 555-0101"
///   ├── MobilePhoneNumber (1, 0x2B) = "(206) 555-0102"
///   ├── Anniversary       (1, 0x05) = "1998-05-23T11:59:00.000Z"
///   ├── Birthday          (1, 0x08) = "1975-11-30T11:59:00.000Z"
///   ├── AssistantName     (1, 0x06) = "Ada Lovelace"
///   ├── AssistantPhoneNumber (1, 0x07) = "(206) 555-0110"
///   ├── Business2PhoneNumber (1, 0x0C) = "(206) 555-0104"
///   ├── BusinessFaxNumber (1, 0x12) = "(206) 555-0105"
///   ├── CarPhoneNumber    (1, 0x14) = "(206) 555-0106"
///   ├── Home2PhoneNumber  (1, 0x20) = "(206) 555-0107"
///   ├── HomeFaxNumber     (1, 0x26) = "(206) 555-0108"
///   ├── PagerNumber       (1, 0x32) = "(206) 555-0109"
///   ├── RadioPhoneNumber  (1, 0x33) = "(206) 555-0111"
///   ├── BusinessAddressStreet (1, 0x11) = "1 Microsoft Way"
///   ├── BusinessAddressCity (1, 0x0D) = "Redmond"
///   ├── BusinessAddressState (1, 0x10) = "WA"
///   ├── BusinessAddressPostalCode (1, 0x0F) = "98052"
///   ├── BusinessAddressCountry (1, 0x0E) = "United States"
///   ├── HomeAddressStreet (1, 0x25) = "42 Pine Street"
///   ├── HomeAddressCity   (1, 0x21) = "Seattle"
///   ├── HomeAddressState  (1, 0x24) = "Washington"
///   ├── HomeAddressPostalCode (1, 0x23) = "98101"
///   ├── HomeAddressCountry (1, 0x22) = "USA"
///   ├── OtherAddressStreet (1, 0x31) = "999 Robson Street"
///   ├── OtherAddressCity  (1, 0x2D) = "Vancouver"
///   ├── OtherAddressState (1, 0x30) = "BC"
///   ├── OtherAddressPostalCode (1, 0x2F) = "V5K 0A1"
///   ├── OtherAddressCountry (1, 0x2E) = "Canada"
///   ├── WebPage           (1, 0x37) = "https://example.com/fzhou"
///   ├── Picture           (1, 0x3C) = <base64 1×1 PNG — DROPPED,
///   │                                   only picture_present survives>
///   ├── ManagerName       (12, 0x0A) = "CN=Bob Stone,OU=Engineering,DC=kylins,DC=local"
///   ├── CompanyMainPhone  (12, 0x0B) = "(206) 555-0100"
///   └── Body              (17, 0x0A)
///         ├── Type        (17, 0x06) = "1"  (PlainText)
///         └── Data        (17, 0x0B) = "Prefers plain-text bodies."
/// ```
/// `pub(crate)` (M8-C task 1) so the class-aware Sync seam tests in
/// `commands/sync.rs` build their Add fixture from this exact tree —
/// one source of truth for the golden wire shape.
pub(crate) fn fixture_full_contact_app_data() -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(PAGE_CONTACTS, CON_FILE_AS, "Zhou, Felix"),
            WbxmlElement::text(PAGE_CONTACTS, CON_FIRST_NAME, "Felix"),
            WbxmlElement::text(PAGE_CONTACTS, CON_MIDDLE_NAME, "Ming"),
            WbxmlElement::text(PAGE_CONTACTS, CON_LAST_NAME, "Zhou"),
            WbxmlElement::text(PAGE_CONTACTS, CON_SUFFIX, "Jr."),
            WbxmlElement::text(PAGE_CONTACTS, CON_TITLE, "Mr."),
            WbxmlElement::text(PAGE_CONTACTS, CON_EMAIL_1, "felixzhou@kylins.local"),
            WbxmlElement::text(PAGE_CONTACTS, CON_EMAIL_2, "felix.zhou@example.com"),
            WbxmlElement::text(PAGE_CONTACTS, CON_EMAIL_3, "felix@home.example"),
            WbxmlElement::text(PAGE_CONTACTS, CON_COMPANY_NAME, "Kylins"),
            WbxmlElement::text(PAGE_CONTACTS, CON_JOB_TITLE, "Development Manager"),
            WbxmlElement::text(PAGE_CONTACTS, CON_BUSINESS_PHONE, "(206) 555-0103"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_PHONE, "(206) 555-0101"),
            WbxmlElement::text(PAGE_CONTACTS, CON_MOBILE_PHONE, "(206) 555-0102"),
            // M8-C task 2 extension (dates kept as raw wire strings —
            // [MS-ASDTYPE] §2.3 dateTime, parse deferred to the
            // conversion layer).
            WbxmlElement::text(PAGE_CONTACTS, CON_ANNIVERSARY, "1998-05-23T11:59:00.000Z"),
            WbxmlElement::text(PAGE_CONTACTS, CON_BIRTHDAY, "1975-11-30T11:59:00.000Z"),
            WbxmlElement::text(PAGE_CONTACTS, CON_ASSISTANT_NAME, "Ada Lovelace"),
            WbxmlElement::text(PAGE_CONTACTS, CON_ASSISTANT_PHONE, "(206) 555-0110"),
            WbxmlElement::text(PAGE_CONTACTS, CON_BUSINESS_2_PHONE, "(206) 555-0104"),
            WbxmlElement::text(PAGE_CONTACTS, CON_BUSINESS_FAX, "(206) 555-0105"),
            WbxmlElement::text(PAGE_CONTACTS, CON_CAR_PHONE, "(206) 555-0106"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_2_PHONE, "(206) 555-0107"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_FAX, "(206) 555-0108"),
            WbxmlElement::text(PAGE_CONTACTS, CON_PAGER, "(206) 555-0109"),
            WbxmlElement::text(PAGE_CONTACTS, CON_RADIO_PHONE, "(206) 555-0111"),
            WbxmlElement::text(
                PAGE_CONTACTS,
                CON_BUSINESS_ADDRESS_STREET,
                "1 Microsoft Way",
            ),
            WbxmlElement::text(PAGE_CONTACTS, CON_BUSINESS_ADDRESS_CITY, "Redmond"),
            WbxmlElement::text(PAGE_CONTACTS, CON_BUSINESS_ADDRESS_STATE, "WA"),
            WbxmlElement::text(PAGE_CONTACTS, CON_BUSINESS_ADDRESS_POSTAL_CODE, "98052"),
            WbxmlElement::text(PAGE_CONTACTS, CON_BUSINESS_ADDRESS_COUNTRY, "United States"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_ADDRESS_STREET, "42 Pine Street"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_ADDRESS_CITY, "Seattle"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_ADDRESS_STATE, "Washington"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_ADDRESS_POSTAL_CODE, "98101"),
            WbxmlElement::text(PAGE_CONTACTS, CON_HOME_ADDRESS_COUNTRY, "USA"),
            WbxmlElement::text(PAGE_CONTACTS, CON_OTHER_ADDRESS_STREET, "999 Robson Street"),
            WbxmlElement::text(PAGE_CONTACTS, CON_OTHER_ADDRESS_CITY, "Vancouver"),
            WbxmlElement::text(PAGE_CONTACTS, CON_OTHER_ADDRESS_STATE, "BC"),
            WbxmlElement::text(PAGE_CONTACTS, CON_OTHER_ADDRESS_POSTAL_CODE, "V5K 0A1"),
            WbxmlElement::text(PAGE_CONTACTS, CON_OTHER_ADDRESS_COUNTRY, "Canada"),
            WbxmlElement::text(PAGE_CONTACTS, CON_WEB_PAGE, "https://example.com/fzhou"),
            // Presence-only in v1: the payload below is dropped at
            // parse time (log::debug!); only `picture_present`
            // survives. See `parse_picture_present`.
            WbxmlElement::text(
                PAGE_CONTACTS,
                CON_PICTURE,
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+\
                 M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
            ),
            // Contacts2 (page 12) elements that appear on Contacts
            // items ([MS-ASWBXML] §2.1.2.1.13).
            WbxmlElement::text(
                pages::CONTACTS2,
                CON2_MANAGER_NAME,
                "CN=Bob Stone,OU=Engineering,DC=kylins,DC=local",
            ),
            WbxmlElement::text(pages::CONTACTS2, CON2_COMPANY_MAIN_PHONE, "(206) 555-0100"),
            WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "1"),
                    WbxmlElement::text(pages::BASE, base::DATA, "Prefers plain-text bodies."),
                ],
            ),
        ],
    )
}

/// Golden props for [`fixture_full_contact_app_data`] — whole-struct
/// equality pins every C1 core field AND every M8-C task 2 field
/// end-to-end (the Picture fixture value intentionally has NO home
/// here: only `picture_present: true` survives the parse).
pub(crate) fn expected_full_contact_props() -> ContactsContactProps {
    ContactsContactProps {
        file_as: Some("Zhou, Felix".to_string()),
        first_name: Some("Felix".to_string()),
        middle_name: Some("Ming".to_string()),
        last_name: Some("Zhou".to_string()),
        name_suffix: Some("Jr.".to_string()),
        name_prefix: Some("Mr.".to_string()),
        email_1: Some("felixzhou@kylins.local".to_string()),
        email_2: Some("felix.zhou@example.com".to_string()),
        email_3: Some("felix@home.example".to_string()),
        company: Some("Kylins".to_string()),
        job_title: Some("Development Manager".to_string()),
        body_plain: Some("Prefers plain-text bodies.".to_string()),
        business_phone: Some("(206) 555-0103".to_string()),
        home_phone: Some("(206) 555-0101".to_string()),
        mobile_phone: Some("(206) 555-0102".to_string()),
        // M8-C task 2 set.
        anniversary: Some("1998-05-23T11:59:00.000Z".to_string()),
        birthday: Some("1975-11-30T11:59:00.000Z".to_string()),
        assistant_name: Some("Ada Lovelace".to_string()),
        manager_name: Some("CN=Bob Stone,OU=Engineering,DC=kylins,DC=local".to_string()),
        assistant_phone: Some("(206) 555-0110".to_string()),
        business_2_phone: Some("(206) 555-0104".to_string()),
        business_fax: Some("(206) 555-0105".to_string()),
        car_phone: Some("(206) 555-0106".to_string()),
        company_main_phone: Some("(206) 555-0100".to_string()),
        home_2_phone: Some("(206) 555-0107".to_string()),
        home_fax: Some("(206) 555-0108".to_string()),
        pager: Some("(206) 555-0109".to_string()),
        radio_phone: Some("(206) 555-0111".to_string()),
        business_address: Some(ContactsAddress {
            street: Some("1 Microsoft Way".to_string()),
            city: Some("Redmond".to_string()),
            state: Some("WA".to_string()),
            postal_code: Some("98052".to_string()),
            country: Some("United States".to_string()),
        }),
        home_address: Some(ContactsAddress {
            street: Some("42 Pine Street".to_string()),
            city: Some("Seattle".to_string()),
            state: Some("Washington".to_string()),
            postal_code: Some("98101".to_string()),
            country: Some("USA".to_string()),
        }),
        other_address: Some(ContactsAddress {
            street: Some("999 Robson Street".to_string()),
            city: Some("Vancouver".to_string()),
            state: Some("BC".to_string()),
            postal_code: Some("V5K 0A1".to_string()),
            country: Some("Canada".to_string()),
        }),
        web_page: Some("https://example.com/fzhou".to_string()),
        picture_present: true,
    }
}

/// GOLDEN: every C1 core field AND every M8-C task 2 field populated
/// with a distinct value. Whole-struct equality assertion — the
/// C1-reviewer pattern.
#[test]
fn parse_full_core_item() {
    let props = parse_contacts_application_data(&fixture_full_contact_app_data())
        .expect("parse must not fail on a well-formed item");
    assert_eq!(props, expected_full_contact_props());
}
