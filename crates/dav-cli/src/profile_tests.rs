//! Tests for profile resolution.
//!
//! Only the parsing and the built-in table are covered here — anything that reads the real
//! profile directory would depend on whoever is running it, and a test that passes because of
//! one developer's `~/.config` is not a test.

use super::*;

#[test]
fn a_plain_file_parses() {
    let values = parse("URL=https://x.test\nUSER=me\nPASS=secret\n");
    assert_eq!(
        values.get("URL").map(String::as_str),
        Some("https://x.test")
    );
    assert_eq!(values.get("USER").map(String::as_str), Some("me"));
    assert_eq!(values.get("PASS").map(String::as_str), Some("secret"));
}

#[test]
fn comments_blank_lines_and_export_are_tolerated() {
    let values = parse("# a note\n\nexport URL=https://x.test\n  USER = me \n");
    assert_eq!(
        values.get("URL").map(String::as_str),
        Some("https://x.test")
    );
    assert_eq!(values.get("USER").map(String::as_str), Some("me"));
}

#[test]
fn surrounding_quotes_are_stripped_but_inner_ones_are_kept() {
    let values = parse("PASS=\"se\"cret\"\nOTHER='quoted'\n");
    assert_eq!(values.get("PASS").map(String::as_str), Some("se\"cret"));
    assert_eq!(values.get("OTHER").map(String::as_str), Some("quoted"));
}

#[test]
fn a_password_containing_an_equals_sign_survives() {
    // Splitting on every `=` instead of the first truncates the password, and the failure is
    // a 401 that looks like a wrong password rather than a mangled one.
    let values = parse("PASS=a=b=c\n");
    assert_eq!(values.get("PASS").map(String::as_str), Some("a=b=c"));
}

#[test]
fn a_line_with_no_equals_is_skipped_rather_than_panicking() {
    let values = parse("URL=https://x.test\ngarbage\n");
    assert_eq!(values.len(), 1);
}

#[test]
fn every_built_in_name_resolves_to_a_profile() {
    // The list and the table are two places that can disagree; a name in `BUILT_INS` with no
    // arm would only surface as "no profile `x`" at the moment someone needed it.
    for name in BUILT_INS {
        let profile = built_in(name).unwrap_or_else(|| panic!("{name} has no built-in arm"));
        assert!(profile.url.starts_with("http://"), "{name}");
        assert!(!profile.user.is_empty(), "{name}");
        assert!(!profile.pass.is_empty(), "{name}");
    }
}

#[test]
fn the_two_stalwart_fixtures_are_different_principals() {
    // The scheduling scenarios need two parties; a copy-paste that pointed both at the same
    // account would make an "invitation" the user sent to themselves.
    let attendee = built_in("stalwart").expect("stalwart");
    let organizer = built_in("stalwart-organizer").expect("stalwart-organizer");
    assert_ne!(attendee.user, organizer.user);
    assert_eq!(attendee.url, organizer.url, "same server, different logins");
}

#[test]
fn an_unknown_name_is_not_a_built_in() {
    assert!(built_in("definitely-not-a-fixture").is_none());
}

#[test]
fn a_configured_fixture_address_wins_over_the_published_port() {
    // The live suites are driven by STALWART_HTTP_ADDR / SABREDAV_HTTP_ADDR; the tool must
    // follow them, or it debugs a different server than the suite that just failed.
    assert_eq!(
        fixture_url(Some("127.0.0.1:9999"), "127.0.0.1:18080"),
        "http://127.0.0.1:9999"
    );
}

#[test]
fn an_unset_fixture_address_falls_back_to_the_published_port() {
    assert_eq!(
        fixture_url(None, "127.0.0.1:18080"),
        "http://127.0.0.1:18080"
    );
}
