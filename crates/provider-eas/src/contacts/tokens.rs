// SPDX-License-Identifier: MPL-2.0
//! MS-ASWBXML Contacts token table for the `contacts` module.
//!
//! Split out of `contacts.rs` (M8-C task 2 fix r1, the 500-line rule) and
//! re-exported there, so every existing `contacts::CON_*` /
//! `contacts::PAGE_CONTACTS` path stays stable.
//!
//! Token fidelity red line: every value below was looked up in
//! `docs/Exchange/MS-ASWBXML.txt` — §2.1.2.1.2 ("Code Page 1: Contacts")
//! and §2.1.2.1.13 ("Code Page 12: Contacts2"), v20220429 — and
//! cross-checked against the `CONTACTS_TOKENS` / `CONTACTS2_TOKENS`
//! tables in `wbxml/code_pages/` (the `contacts_token_constants_match_spec`
//! test in tests/commands_contacts.rs pins both directions). Never from
//! memory. Element value semantics per [MS-ASCNTC] §2.2.2 (all modeled
//! fields are string data type, [MS-ASDTYPE] §2.7, except Anniversary/
//! Birthday which are dateTime on the wire and kept as raw strings).

/// Code page 1 = Contacts ([MS-ASWBXML] §2.1.2.1.2).
pub const PAGE_CONTACTS: u8 = 1;

// --- Page 1 (Contacts) tokens — [MS-ASWBXML] §2.1.2.1.2 table -------------
// (docs/Exchange/MS-ASWBXML.txt, "2.1.2.1.2 Code Page 1: Contacts";
// 0x09/0x0A/0x0B are the 2.5-only contacts Body/BodySize/BodyTruncated —
// with 12.0+ `airsyncbase:Body` (page 17) replaces them ([MS-ASWBXML]
// §2.1.2.1.2 note 1), so they are not modeled here; 0x3B is unassigned.)
//
// M8-C task 2 models the remaining practical set (addresses, phones,
// dates, assistant/manager, web page, picture presence), including the
// two Contacts2 (page 12) elements that appear on Contacts items
// (ManagerName, CompanyMainPhone — [MS-ASWBXML] §2.1.2.1.13).
//
// CANONICAL SKIP LIST — still unmodeled, debug-skipped by the parse
// catch-all in `super::parse_contacts_application_data` (pinned by
// `exotic_contact_elements_are_skipped` in tests/commands_contacts.rs):
// page 1 — Categories/Category (0x15/0x16), Children/Child (0x17/0x18),
// Department (0x1A), OfficeLocation (0x2C), Spouse (0x34),
// YomiCompanyName/YomiFirstName/YomiLastName (0x38/0x39/0x3A),
// Alias (0x3D), WeightedRank (0x3E); Contacts2 page 12 — CustomerId
// (0x05), GovernmentId (0x06), IMAddress/IMAddress2/IMAddress3
// (0x07/0x08/0x09), AccountName (0x0C), NickName (0x0D), MMS (0x0E).

/// `FileAs` = 0x1E (all versions). How the contact is filed in the Contacts
/// folder ([MS-ASCNTC] §2.2.2.30). String.
pub const CON_FILE_AS: u8 = 0x1E;
/// `FirstName` = 0x1F (all versions). The contact's first name
/// ([MS-ASCNTC] §2.2.2.31). String.
pub const CON_FIRST_NAME: u8 = 0x1F;
/// `Email1Address` = 0x1B (all versions). The FIRST e-mail address for the
/// contact ([MS-ASCNTC] §2.2.2.27). String.
pub const CON_EMAIL_1: u8 = 0x1B;
/// `Email2Address` = 0x1C (all versions). The second e-mail address
/// ([MS-ASCNTC] §2.2.2.28). String.
pub const CON_EMAIL_2: u8 = 0x1C;
/// `Email3Address` = 0x1D (all versions). The third e-mail address
/// ([MS-ASCNTC] §2.2.2.29). String.
pub const CON_EMAIL_3: u8 = 0x1D;
/// `CompanyName` = 0x19 (all versions). The company name for the contact
/// ([MS-ASCNTC] §2.2.2.24). String.
pub const CON_COMPANY_NAME: u8 = 0x19;
/// `JobTitle` = 0x28 (all versions). The contact's job title
/// ([MS-ASCNTC] §2.2.2.44). String.
pub const CON_JOB_TITLE: u8 = 0x28;
/// `BusinessPhoneNumber` = 0x13 (all versions). The PRIMARY business phone
/// number ([MS-ASCNTC] §2.2.2.16). String; Business2PhoneNumber (0x0C) is
/// the second line and lands in M8-C task 2.
pub const CON_BUSINESS_PHONE: u8 = 0x13;
/// `HomePhoneNumber` = 0x27 (all versions). The home phone number
/// ([MS-ASCNTC] §2.2.2.39). String; Home2PhoneNumber (0x20) lands in
/// M8-C task 2.
pub const CON_HOME_PHONE: u8 = 0x27;
/// `MobilePhoneNumber` = 0x2B (all versions). The mobile phone number
/// ([MS-ASCNTC] §2.2.2.49). String.
pub const CON_MOBILE_PHONE: u8 = 0x2B;
/// `LastName` = 0x29 (all versions). The contact's last name
/// ([MS-ASCNTC] §2.2.2.45). String.
pub const CON_LAST_NAME: u8 = 0x29;
/// `MiddleName` = 0x2A (all versions). The contact's middle name
/// ([MS-ASCNTC] §2.2.2.47). String.
pub const CON_MIDDLE_NAME: u8 = 0x2A;
/// `Suffix` = 0x35 (all versions). The suffix for the contact's name, e.g.
/// "Jr." ([MS-ASCNTC] §2.2.2.61). String.
pub const CON_SUFFIX: u8 = 0x35;
/// `Title` = 0x36 (all versions). The contact's business title — the name
/// prefix ("Mr.", "Dr.") in Outlook's contact model, distinct from
/// `JobTitle` ([MS-ASCNTC] §2.2.2.62). String.
pub const CON_TITLE: u8 = 0x36;

// --- Page 1 tokens added by M8-C task 2 ------------------------------------

/// `Anniversary` = 0x05 (all versions). Wedding anniversary date
/// ([MS-ASCNTC] §2.2.2.3). dateTime on the wire ([MS-ASDTYPE] §2.3);
/// kept as the raw string — date parsing is the conversion layer's job.
pub const CON_ANNIVERSARY: u8 = 0x05;
/// `AssistantName` = 0x06 (all versions). The name of the contact's
/// assistant ([MS-ASCNTC] §2.2.2.4). String.
pub const CON_ASSISTANT_NAME: u8 = 0x06;
/// `AssistantPhoneNumber` = 0x07 (all versions). The assistant's phone
/// number ([MS-ASCNTC] §2.2.2.5). String.
pub const CON_ASSISTANT_PHONE: u8 = 0x07;
/// `Birthday` = 0x08 (all versions). The contact's birth date
/// ([MS-ASCNTC] §2.2.2.6). dateTime on the wire ([MS-ASDTYPE] §2.3);
/// kept raw, like `Anniversary`.
pub const CON_BIRTHDAY: u8 = 0x08;
/// `Business2PhoneNumber` = 0x0C (all versions). The second business
/// line ([MS-ASCNTC] §2.2.2.17). String.
pub const CON_BUSINESS_2_PHONE: u8 = 0x0C;
/// `BusinessAddressCity` = 0x0D (all versions). The business city
/// ([MS-ASCNTC] §2.2.2.10). String.
pub const CON_BUSINESS_ADDRESS_CITY: u8 = 0x0D;
/// `BusinessAddressCountry` = 0x0E (all versions). The business
/// country/region ([MS-ASCNTC] §2.2.2.11). String.
pub const CON_BUSINESS_ADDRESS_COUNTRY: u8 = 0x0E;
/// `BusinessAddressPostalCode` = 0x0F (all versions). The business
/// postal code ([MS-ASCNTC] §2.2.2.12). String.
pub const CON_BUSINESS_ADDRESS_POSTAL_CODE: u8 = 0x0F;
/// `BusinessAddressState` = 0x10 (all versions). The business state
/// ([MS-ASCNTC] §2.2.2.13). String.
pub const CON_BUSINESS_ADDRESS_STATE: u8 = 0x10;
/// `BusinessAddressStreet` = 0x11 (all versions). The business street
/// address ([MS-ASCNTC] §2.2.2.14). String.
pub const CON_BUSINESS_ADDRESS_STREET: u8 = 0x11;
/// `BusinessFaxNumber` = 0x12 (all versions). The business fax number
/// ([MS-ASCNTC] §2.2.2.15). String.
pub const CON_BUSINESS_FAX: u8 = 0x12;
/// `CarPhoneNumber` = 0x14 (all versions). The car phone number
/// ([MS-ASCNTC] §2.2.2.18). String.
pub const CON_CAR_PHONE: u8 = 0x14;
/// `Home2PhoneNumber` = 0x20 (all versions). The second home line
/// ([MS-ASCNTC] §2.2.2.40). String.
pub const CON_HOME_2_PHONE: u8 = 0x20;
/// `HomeAddressCity` = 0x21 (all versions). The home city
/// ([MS-ASCNTC] §2.2.2.33). String.
pub const CON_HOME_ADDRESS_CITY: u8 = 0x21;
/// `HomeAddressCountry` = 0x22 (all versions). The home country/region
/// ([MS-ASCNTC] §2.2.2.34). String.
pub const CON_HOME_ADDRESS_COUNTRY: u8 = 0x22;
/// `HomeAddressPostalCode` = 0x23 (all versions). The home postal code
/// ([MS-ASCNTC] §2.2.2.35). String.
pub const CON_HOME_ADDRESS_POSTAL_CODE: u8 = 0x23;
/// `HomeAddressState` = 0x24 (all versions). The home state
/// ([MS-ASCNTC] §2.2.2.36). String.
pub const CON_HOME_ADDRESS_STATE: u8 = 0x24;
/// `HomeAddressStreet` = 0x25 (all versions). The home street address
/// ([MS-ASCNTC] §2.2.2.37). String.
pub const CON_HOME_ADDRESS_STREET: u8 = 0x25;
/// `HomeFaxNumber` = 0x26 (all versions). The home fax number
/// ([MS-ASCNTC] §2.2.2.38). String.
pub const CON_HOME_FAX: u8 = 0x26;
/// `OtherAddressCity` = 0x2D (all versions). The alternate city
/// ([MS-ASCNTC] §2.2.2.52). String.
pub const CON_OTHER_ADDRESS_CITY: u8 = 0x2D;
/// `OtherAddressCountry` = 0x2E (all versions). The alternate
/// country/region ([MS-ASCNTC] §2.2.2.53). String.
pub const CON_OTHER_ADDRESS_COUNTRY: u8 = 0x2E;
/// `OtherAddressPostalCode` = 0x2F (all versions). The alternate postal
/// code ([MS-ASCNTC] §2.2.2.54). String.
pub const CON_OTHER_ADDRESS_POSTAL_CODE: u8 = 0x2F;
/// `OtherAddressState` = 0x30 (all versions). The alternate state
/// ([MS-ASCNTC] §2.2.2.55). String.
pub const CON_OTHER_ADDRESS_STATE: u8 = 0x30;
/// `OtherAddressStreet` = 0x31 (all versions). The alternate street
/// address ([MS-ASCNTC] §2.2.2.56). String.
pub const CON_OTHER_ADDRESS_STREET: u8 = 0x31;
/// `PagerNumber` = 0x32 (all versions). The pager number
/// ([MS-ASCNTC] §2.2.2.57). String.
pub const CON_PAGER: u8 = 0x32;
/// `RadioPhoneNumber` = 0x33 (all versions). The radio phone number
/// ([MS-ASCNTC] §2.2.2.59). String.
pub const CON_RADIO_PHONE: u8 = 0x33;
/// `WebPage` = 0x37 (all versions). The contact's web site or personal
/// web page ([MS-ASCNTC] §2.2.2.63). String.
pub const CON_WEB_PAGE: u8 = 0x37;
/// `Picture` = 0x3C (all versions). The contact picture — a base64
/// stream, ≤ 48 KB ([MS-ASCNTC] §2.2.2.58). v1 models PRESENCE only
/// (`picture_present`); the payload is dropped at parse time.
pub const CON_PICTURE: u8 = 0x3C;

// --- Page 12 (Contacts2) tokens on Contacts items — [MS-ASWBXML]
// §2.1.2.1.13 (docs/Exchange/MS-ASWBXML.txt, "Code Page 12: Contacts2").

/// `contacts2:ManagerName` = page 12, 0x0A (all versions). The
/// distinguished name (DN) of the contact's manager ([MS-ASCNTC]
/// §2.2.2.46). String.
pub const CON2_MANAGER_NAME: u8 = 0x0A;
/// `contacts2:CompanyMainPhone` = page 12, 0x0B (all versions). The main
/// phone number of the contact's company ([MS-ASCNTC] §2.2.2.23). String.
pub const CON2_COMPANY_MAIN_PHONE: u8 = 0x0B;
