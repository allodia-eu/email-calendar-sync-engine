// SPDX-License-Identifier: MPL-2.0
//! Contacts ApplicationData per-field parses: addresses, phones, dates, names, picture, email
//! forms.

use super::*;

// ============================================================================
// Address sets ([MS-ASCNTC] §2.2.2.10-.14, §2.2.2.33-.37, §2.2.2.52-.56)
// ============================================================================

/// Business-address complete round: all five components
/// ([MS-ASWBXML] §2.1.2.1.2 tokens 0x11 Street, 0x0D City, 0x10 State,
/// 0x0F PostalCode, 0x0E Country) parse into one `ContactsAddress`.
#[test]
fn business_address_full_round() {
    let tree = app_data(vec![
        con(CON_BUSINESS_ADDRESS_STREET, "1 Microsoft Way"),
        con(CON_BUSINESS_ADDRESS_CITY, "Redmond"),
        con(CON_BUSINESS_ADDRESS_STATE, "WA"),
        con(CON_BUSINESS_ADDRESS_POSTAL_CODE, "98052"),
        con(CON_BUSINESS_ADDRESS_COUNTRY, "United States"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(
        props.business_address,
        Some(ContactsAddress {
            street: Some("1 Microsoft Way".to_string()),
            city: Some("Redmond".to_string()),
            state: Some("WA".to_string()),
            postal_code: Some("98052".to_string()),
            country: Some("United States".to_string()),
        }),
        "all five BusinessAddress components must survive the round"
    );
    assert_eq!(props.home_address, None, "home set untouched");
    assert_eq!(props.other_address, None, "other set untouched");
}

/// The three address sets are independent wire groups: a PARTIAL home set
/// (street + city only — every component "can be ghosted", [MS-ASCNTC]
/// §2.2.2.10 et al.) yields a partial `ContactsAddress`, while the other
/// set round-trips complete beside it.
#[test]
fn home_and_other_address_sets_parse_independently() {
    let tree = app_data(vec![
        con(CON_HOME_ADDRESS_STREET, "42 Pine Street"),
        con(CON_HOME_ADDRESS_CITY, "Seattle"),
        con(CON_OTHER_ADDRESS_STREET, "999 Robson Street"),
        con(CON_OTHER_ADDRESS_CITY, "Vancouver"),
        con(CON_OTHER_ADDRESS_STATE, "BC"),
        con(CON_OTHER_ADDRESS_POSTAL_CODE, "V5K 0A1"),
        con(CON_OTHER_ADDRESS_COUNTRY, "Canada"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(
        props.home_address,
        Some(ContactsAddress {
            street: Some("42 Pine Street".to_string()),
            city: Some("Seattle".to_string()),
            state: None,
            postal_code: None,
            country: None,
        }),
        "absent address components stay None inside the set"
    );
    assert_eq!(
        props.other_address,
        Some(ContactsAddress {
            street: Some("999 Robson Street".to_string()),
            city: Some("Vancouver".to_string()),
            state: Some("BC".to_string()),
            postal_code: Some("V5K 0A1".to_string()),
            country: Some("Canada".to_string()),
        })
    );
    assert_eq!(props.business_address, None, "business set untouched");
}

// ============================================================================
// Remaining practical phones ([MS-ASCNTC] §2.2.2.5, .15, .17, .18, .23,
// .38, .40, .57, .59)
// ============================================================================

/// All nine task-2 phone slots parse per-field (one item, distinct values).
/// CompanyMainPhone and ManagerName live on the Contacts2 page (12) but
/// appear on Contacts-class items ([MS-ASWBXML] §2.1.2.1.13).
#[test]
fn remaining_phones_parse_per_field() {
    let tree = app_data(vec![
        con(CON_ASSISTANT_PHONE, "(206) 555-0110"),
        con(CON_BUSINESS_2_PHONE, "(206) 555-0104"),
        con(CON_BUSINESS_FAX, "(206) 555-0105"),
        con(CON_CAR_PHONE, "(206) 555-0106"),
        con(CON_HOME_2_PHONE, "(206) 555-0107"),
        con(CON_HOME_FAX, "(206) 555-0108"),
        con(CON_PAGER, "(206) 555-0109"),
        con(CON_RADIO_PHONE, "(206) 555-0111"),
        WbxmlElement::text(pages::CONTACTS2, CON2_COMPANY_MAIN_PHONE, "(206) 555-0100"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(props.assistant_phone.as_deref(), Some("(206) 555-0110"));
    assert_eq!(props.business_2_phone.as_deref(), Some("(206) 555-0104"));
    assert_eq!(props.business_fax.as_deref(), Some("(206) 555-0105"));
    assert_eq!(props.car_phone.as_deref(), Some("(206) 555-0106"));
    assert_eq!(props.home_2_phone.as_deref(), Some("(206) 555-0107"));
    assert_eq!(props.home_fax.as_deref(), Some("(206) 555-0108"));
    assert_eq!(props.pager.as_deref(), Some("(206) 555-0109"));
    assert_eq!(props.radio_phone.as_deref(), Some("(206) 555-0111"));
    assert_eq!(props.company_main_phone.as_deref(), Some("(206) 555-0100"));
    // C1 phone slots stay untouched.
    assert_eq!(props.business_phone, None);
    assert_eq!(props.home_phone, None);
    assert_eq!(props.mobile_phone, None);
}

// ============================================================================
// Anniversary / Birthday — RAW wire strings ([MS-ASCNTC] §2.2.2.3, §2.2.2.6)
// ============================================================================

/// Anniversary and Birthday are dateTime on the wire ([MS-ASDTYPE] §2.3:
/// `YYYY-MM-DDTHH:MM:SS.MSSZ`, UTC; the time part "might be 11:59 and
/// SHOULD be ignored"). The parser keeps the wire string RAW — no date
/// parsing here; interpretation belongs to the conversion layer. Pin the
/// exact byte-for-byte retention.
#[test]
fn anniversary_and_birthday_keep_raw_wire_strings() {
    let tree = app_data(vec![
        con(CON_ANNIVERSARY, "1998-05-23T11:59:00.000Z"),
        con(CON_BIRTHDAY, "1975-11-30T11:59:00.000Z"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(
        props.anniversary.as_deref(),
        Some("1998-05-23T11:59:00.000Z"),
        "Anniversary must be retained verbatim (no date parse)"
    );
    assert_eq!(
        props.birthday.as_deref(),
        Some("1975-11-30T11:59:00.000Z"),
        "Birthday must be retained verbatim (no date parse)"
    );
    // A server that deviates from the spec shape still round-trips: the
    // raw-string contract does not depend on the value's date-ness.
    let odd = app_data(vec![con(CON_BIRTHDAY, "not-a-date")]);
    let props = parse_contacts_application_data(&odd).expect("parse ok");
    assert_eq!(props.birthday.as_deref(), Some("not-a-date"));
}

// ============================================================================
// Assistant / manager / web page
// ============================================================================

/// AssistantName (page 1, 0x06), ManagerName (Contacts2 page 12, 0x0A —
/// the manager's DN, [MS-ASCNTC] §2.2.2.46), and WebPage (page 1, 0x37).
#[test]
fn assistant_manager_and_web_page_parse() {
    let tree = app_data(vec![
        con(CON_ASSISTANT_NAME, "Ada Lovelace"),
        WbxmlElement::text(
            pages::CONTACTS2,
            CON2_MANAGER_NAME,
            "CN=Bob Stone,OU=Engineering,DC=kylins,DC=local",
        ),
        con(CON_WEB_PAGE, "https://example.com/fzhou"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(props.assistant_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(
        props.manager_name.as_deref(),
        Some("CN=Bob Stone,OU=Engineering,DC=kylins,DC=local")
    );
    assert_eq!(props.web_page.as_deref(), Some("https://example.com/fzhou"));
}

// ============================================================================
// Picture — presence only, value dropped ([MS-ASCNTC] §2.2.2.58)
// ============================================================================

/// A 1×1 PNG in base64 — plausible Picture payload ([MS-ASCNTC] §2.2.2.58:
/// base64 stream, ≤ 48 KB).
const PICTURE_PAYLOAD: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+\
M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// PIN: the Picture bytes are NOT retained anywhere. v1 models
/// `picture_present: bool` only; the base64 payload is dropped at parse
/// time (with a `log::debug!`). Whole-struct equality against a props
/// value that has no home for the payload proves nothing leaked — and the
/// Debug-render check makes the pin explicit.
#[test]
fn picture_value_is_dropped_only_presence_kept() {
    let tree = app_data(vec![
        con(CON_FILE_AS, "Zhou, Felix"),
        con(CON_PICTURE, PICTURE_PAYLOAD),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert!(
        props.picture_present,
        "a Picture element with a payload must set picture_present"
    );
    // Whole-struct equality: the ONLY trace of the Picture element is the
    // boolean — there is no field that could smuggle the payload through.
    assert_eq!(
        props,
        ContactsContactProps {
            file_as: Some("Zhou, Felix".to_string()),
            picture_present: true,
            ..Default::default()
        },
        "props must carry presence only, not the picture bytes"
    );
    // Belt-and-braces: the payload appears nowhere in the model's render.
    let rendered = format!("{props:?}");
    assert!(
        !rendered.contains("iVBORw0KGgo"),
        "picture payload must not be retained in any field"
    );
}

/// A Picture element WITHOUT a value (empty element — the cleared/ghosted
/// wire shape) counts as no picture: `picture_present` stays false.
#[test]
fn picture_without_value_counts_as_absent() {
    let tree = app_data(vec![
        con(CON_FILE_AS, "Kerry, Anat"),
        WbxmlElement::empty(PAGE_CONTACTS, CON_PICTURE),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert!(
        !props.picture_present,
        "an empty Picture element is a cleared picture, not a present one"
    );
    assert_eq!(props.file_as.as_deref(), Some("Kerry, Anat"));
}

// ============================================================================
// E-mail display-name forms (M8-C1, 2026-08-17 live seed drill)
// ============================================================================

/// RED (M8-C1): Exchange/OWA serializes EmailAddress values in the
/// RFC 5322 mailbox form `"Display Name" <local@domain>` (observed live:
/// the seeded OWA contact synced with the whole quoted string as its
/// only e-mail, breaking the contacts.email UNIQUE dedup, composer
/// autocomplete, and vCard EMAIL export). The parse boundary must store
/// the BARE address; a plain address passes through unchanged.
#[test]
fn email_display_name_forms_parse_to_bare_address() {
    let tree = app_data(vec![
        con(CON_EMAIL_1, "\"Zhou, Felix\" <seed.contact@kylins.local>"),
        con(CON_EMAIL_2, "Felix Zhou <felix.zhou@example.com>"),
        con(CON_EMAIL_3, "plain@x.y"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(
        props.email_1.as_deref(),
        Some("seed.contact@kylins.local"),
        "quoted display-name form must yield the bare address"
    );
    assert_eq!(
        props.email_2.as_deref(),
        Some("felix.zhou@example.com"),
        "unquoted display-name form must yield the bare address"
    );
    assert_eq!(
        props.email_3.as_deref(),
        Some("plain@x.y"),
        "a plain address must pass through unchanged"
    );
}

/// RED (M8-C1): an unclosed angle bracket is malformed — the value must
/// be REJECTED, not stored with the stray bracket glued on (today
/// `Name <a@b.c` passes the bare `contains('@')` shape check and lands
/// in the email column verbatim).
#[test]
fn email_unclosed_bracket_value_is_rejected() {
    let tree = app_data(vec![
        con(CON_FILE_AS, "Bracket, Unbalanced"),
        con(CON_EMAIL_1, "Name <a@b.c"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(
        props.email_1, None,
        "an unclosed angle bracket must not survive into the e-mail field"
    );
    assert_eq!(props.file_as.as_deref(), Some("Bracket, Unbalanced"));
}

/// PIN (M8-C1): a bracketed value that is NOT an address must never be
/// mangled into one — the field degrades to None (the existing SMTP-shape
/// gate), and no address is invented from the display-name part.
#[test]
fn email_bracketed_non_address_is_not_invented() {
    for bad in ["<no-at>", "Display Name <not-an-address>"] {
        let tree = app_data(vec![con(CON_EMAIL_1, bad)]);
        let props = parse_contacts_application_data(&tree).expect("parse ok");
        assert_eq!(
            props.email_1, None,
            "\"{bad}\" must not turn into a fabricated address"
        );
    }
}

// ============================================================================
// Malformed / ghosted + exotic-skip discipline
// ============================================================================

/// Empty-valued elements are legitimate wire data (ghosted/cleared values,
/// C1 review note). The new string fields degrade to `None`; a ghosted
/// address component still opens its address set with the component `None`.
/// No panic anywhere.
#[test]
fn empty_valued_new_fields_degrade_to_none() {
    let tree = app_data(vec![
        WbxmlElement::empty(PAGE_CONTACTS, CON_WEB_PAGE),
        WbxmlElement::empty(PAGE_CONTACTS, CON_ASSISTANT_PHONE),
        WbxmlElement::empty(PAGE_CONTACTS, CON_ANNIVERSARY),
        WbxmlElement::empty(PAGE_CONTACTS, CON_BUSINESS_ADDRESS_STREET),
        con(CON_BUSINESS_ADDRESS_CITY, "Redmond"),
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(props.web_page, None);
    assert_eq!(props.assistant_phone, None);
    assert_eq!(props.anniversary, None);
    assert_eq!(
        props.business_address,
        Some(ContactsAddress {
            street: None,
            city: Some("Redmond".to_string()),
            state: None,
            postal_code: None,
            country: None,
        }),
        "a ghosted street component still opens the business address set"
    );
    assert!(!props.picture_present);
}

/// Exotic Contacts/Contacts2 elements this task deliberately does NOT
/// model are skipped without panic and without disturbing modeled fields:
/// Spouse, Children/Child, Department, OfficeLocation, YomiFirstName,
/// Alias, WeightedRank (page 1) and CustomerId, GovernmentId, IMAddress,
/// AccountName, NickName, MMS (Contacts2 page 12). Documented skip list —
/// see `src/contacts.rs` parse-entry doc.
#[test]
fn exotic_contact_elements_are_skipped() {
    let tree = app_data(vec![
        con(CON_FILE_AS, "Real FileAs"),
        con(0x34, "Jamie Zhou"), // Spouse
        WbxmlElement::container(
            PAGE_CONTACTS,
            0x17,                         // Children
            vec![con(0x18, "Lily Zhou")], // Child
        ),
        con(0x1A, "Engineering"),                               // Department
        con(0x2C, "B-2042"),                                    // OfficeLocation
        con(0x39, "フェリクス"),                                // YomiFirstName
        con(0x3D, "fzhou"),                                     // Alias
        con(0x3E, "100"),                                       // WeightedRank
        WbxmlElement::text(pages::CONTACTS2, 0x05, "cust-1"),   // CustomerId
        WbxmlElement::text(pages::CONTACTS2, 0x06, "gov-1"),    // GovernmentId
        WbxmlElement::text(pages::CONTACTS2, 0x07, "fzhou@im"), // IMAddress
        WbxmlElement::text(pages::CONTACTS2, 0x0C, "kylins"),   // AccountName
        WbxmlElement::text(pages::CONTACTS2, 0x0D, "Felix"),    // NickName
        WbxmlElement::text(pages::CONTACTS2, 0x0E, "mms@x"),    // MMS
    ]);
    let props = parse_contacts_application_data(&tree).expect("parse ok");
    assert_eq!(
        props,
        ContactsContactProps {
            file_as: Some("Real FileAs".to_string()),
            ..Default::default()
        },
        "exotic elements must leave every modeled field at its default"
    );
}
