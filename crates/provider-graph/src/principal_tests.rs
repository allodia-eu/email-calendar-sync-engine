//! The principal's URL root, and the address check that guards it.

use engine_core::error::FailureClass;
use engine_provider::ProviderError;

use super::*;

fn root_of(address: &str) -> String {
    MailboxPrincipal::user(address).unwrap().root()
}

#[test]
fn principal_roots_match_the_graph_url_shape() {
    assert_eq!(MailboxPrincipal::Me.root(), "/me");
    // Graph's own documented shared-mailbox URL carries the address verbatim.
    assert_eq!(root_of("info@example.org"), "/users/info@example.org");
    assert_eq!(
        root_of("first.last+tag@example.org"),
        "/users/first.last+tag@example.org"
    );
    assert_eq!(MailboxPrincipal::Me.address(), None);
    assert_eq!(
        MailboxPrincipal::user("info@example.org")
            .unwrap()
            .address(),
        Some("info@example.org")
    );
}

#[test]
fn a_path_separator_is_refused_because_graph_reparses_it_after_decoding() {
    // Observed against a real tenant: an encoded `/` inside the segment became a path
    // separator server-side. Encoding cannot contain it, so it never gets that far — and
    // neither does `\`, which Graph treated as a separator in the same probe.
    for hostile in [
        "../me",
        "a@b.example/../../me",
        "a@b.example/mailFolders/sentitems",
        "a@b.example\\me",
        "..\\me@b.example",
    ] {
        let err = MailboxPrincipal::user(hostile).unwrap_err();
        assert!(matches!(err, GraphError::InvalidAddress(_)), "{hostile}");
        assert_eq!(err.failure_class(), FailureClass::Permanent, "{hostile}");
    }
}

#[test]
fn something_that_is_not_one_address_is_refused() {
    for malformed in [
        "",
        "no-at-sign",
        "@example.org",
        "local@",
        "a@b@example.org",
        "a b@example.org",
        "a@example.org\r\nX-Injected: yes",
    ] {
        assert!(MailboxPrincipal::user(malformed).is_err(), "{malformed:?}");
    }
}

#[test]
fn every_other_character_is_encoded_and_arrives_as_data() {
    // What the tenant probe showed Graph reading as part of the name once encoded, rather
    // than as URL structure. None of these can re-point a request.
    assert_eq!(
        root_of("a@b.example?$select=id"),
        "/users/a@b.example%3F%24select%3Did"
    );
    assert_eq!(root_of("a@b.example#frag"), "/users/a@b.example%23frag");
    assert_eq!(root_of("a%2Fb@example.org"), "/users/a%252Fb@example.org");
    // An Entra guest UPN really is shaped like this, and must still work.
    assert_eq!(
        root_of("user_partner.example#EXT#@tenant.example"),
        "/users/user_partner.example%23EXT%23@tenant.example"
    );
    // Non-ASCII travels as its UTF-8 bytes.
    assert_eq!(root_of("jörg@example.org"), "/users/j%C3%B6rg@example.org");
}

#[test]
fn a_stored_handle_binds_the_mailbox_it_names_and_is_rechecked() {
    let handle = SharedMailboxId::try_from("shared@example.test").unwrap();
    assert_eq!(
        MailboxPrincipal::shared(&handle).unwrap().root(),
        "/users/shared@example.test"
    );
    // A handle is stored by the host between discovery and binding; one this adapter would
    // never have produced is refused, not spliced.
    let tampered = SharedMailboxId::try_from("shared@example.test/../me").unwrap();
    assert!(MailboxPrincipal::shared(&tampered).is_err());
}

#[test]
fn the_refusal_reaches_a_provider_caller_as_permanent_without_echoing_the_input() {
    let err: ProviderError = MailboxPrincipal::user("x@example.org\nX-Injected: yes")
        .unwrap_err()
        .into();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.detail().contains("U+000A"), "{err}");
    assert!(!err.detail().contains("X-Injected"), "{err}");
}
