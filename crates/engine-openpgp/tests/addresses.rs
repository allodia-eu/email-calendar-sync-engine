//! Which address a User ID binds, and how addresses compare (openpgp.md).

mod support;

use engine_openpgp::{Certificate, canonical_address};
use pgp::types::KeyVersion;
use support::{
    at,
    forge::{Cert, Forge, Sig, user_id},
    secs,
};

#[test]
fn addresses_compare_by_autocrypt_canonicalisation() {
    // Autocrypt Level 1: IDNA domain, lowercased local part.
    for (input, canonical) in [
        ("alice@example.org", "alice@example.org"),
        ("  Alice@EXAMPLE.org ", "alice@example.org"),
        ("ÖLAF@Bücher.example", "ölaf@xn--bcher-kva.example"),
    ] {
        assert_eq!(
            canonical_address(input).as_deref(),
            Some(canonical),
            "{input}"
        );
    }
    for input in [
        "",
        "alice",
        "@example.org",
        "alice@",
        "a b@example.org",
        "a@b@example.org",
    ] {
        assert_eq!(canonical_address(input), None, "{input}");
    }
}

#[test]
fn a_user_id_binds_the_address_it_names_once() {
    let forms = [
        ("Alice <alice@example.org>", Some("alice@example.org")),
        ("bob@example.org", Some("bob@example.org")),
        (
            "Carol (work) <Carol@Example.org>",
            Some("carol@example.org"),
        ),
        (
            "\"Smith, Dave\" <dave@example.org>",
            Some("dave@example.org"),
        ),
        // A second User ID for an address already bound adds nothing.
        ("Alice at home <ALICE@example.org>", None),
        ("Erin Smith", None),
        ("Frank <>", None),
        ("Grace <not an address>", None),
    ];
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let mut cert = Cert::new(&key);
    for (text, _) in forms {
        let uid = user_id(text);
        let signature = forge.certify(&key, &uid, Sig::at(secs("2025-01-01")).flags(0x03));
        cert = cert.user(uid, vec![signature]);
    }
    let facts = Certificate::parse(&cert.bytes())
        .expect("parses")
        .at(at("2025-03-01"))
        .facts();
    let expected: Vec<&str> = forms.iter().filter_map(|(_, address)| *address).collect();
    assert_eq!(facts.addresses, expected);
}
