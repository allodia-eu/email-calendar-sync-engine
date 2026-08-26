// SPDX-License-Identifier: MPL-2.0
// Per-behavior Contacts parser tests (golden fixtures live in
// contacts_testutil; the token-spec test in tests/commands_contacts.rs).

// `pub(crate)` (M8-C task 1): visibility only. M8-C task 2 fix r1 moved
// the golden fixture pair + the golden whole-struct test to
// `crate::contacts_testutil` (shared with the Sync seam tests in
// `commands/sync/tests.rs`) and the token-spec test to
// tests/commands_contacts.rs; the per-behavior parser tests stay here.

use super::{parse::extract_bare_address, *};
use crate::{
    commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
    wbxml::{
        WbxmlElement,
        tags::{base, pages},
    },
};

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
