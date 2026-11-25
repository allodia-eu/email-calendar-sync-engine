//! [`AuthenticatedClient::admits`]: the same answer `send` gives before it asks, given ahead.

use std::sync::Arc;

use reqwest::Url;

use super::tests::{Tokens, account};

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

#[test]
fn an_https_origin_the_source_permits_is_admitted() {
    let source = Arc::new(Tokens::new("A", &[]));
    let client = account(&source, "https://dav.example.com/");
    assert!(client.admits(&url("https://dav.example.com/cal/")));
    assert!(client.admits(&url("https://p1.dav.example.com:8443/")));
    assert_eq!(
        *source.checked.lock().unwrap(),
        [
            engine_provider::Origin::new("https", "dav.example.com", 443),
            engine_provider::Origin::new("https", "p1.dav.example.com", 8443),
        ],
        "the source is asked about each origin, and for nothing else"
    );
}

#[test]
fn an_origin_the_source_does_not_permit_is_not_admitted() {
    let source = Arc::new(Tokens {
        permits: false,
        ..Tokens::new("A", &[])
    });
    let client = account(&source, "https://dav.example.com/");
    assert!(!client.admits(&url("https://dav.example.com/")));
}

#[test]
fn plaintext_is_admitted_only_from_loopback_to_loopback() {
    let source = Arc::new(Tokens::new("A", &[]));
    let loopback = account(&source, "http://127.0.0.1:18082/");
    assert!(loopback.admits(&url("http://127.0.0.1:18082/remote.php/dav/")));
    assert!(loopback.admits(&url("http://localhost:9/")));
    assert!(!loopback.admits(&url("http://dav.example.com/")));

    let remote = account(&source, "http://dav.example.com/");
    assert!(
        !remote.admits(&url("http://dav.example.com/")),
        "configured, but in clear"
    );

    let tls = account(&source, "https://dav.example.com/");
    assert!(
        !tls.admits(&url("http://127.0.0.1:18082/")),
        "an account on TLS never sends in clear, loopback included"
    );
    assert_eq!(
        source.checked.lock().unwrap().len(),
        2,
        "the source is asked only about origins the plaintext rule let through"
    );
}

/// The plaintext rule's refusal says whose address is at fault: the account's, where its
/// configured address is itself plaintext off loopback (`Insecure`), or the server's, where it
/// led an account whose address the rule accepts to plaintext (`LedToInsecure`).
#[test]
fn a_plaintext_refusal_says_whether_the_configured_address_or_the_server_is_at_fault() {
    use engine_provider::CredentialFailureKind::{Insecure, LedToInsecure};
    let source = Arc::new(Tokens::new("A", &[]));
    let kind = |configured: &str, to: &str| {
        account(&source, configured)
            .admit(&url(to))
            .map_err(|refused| refused.into_error().credential_failure())
    };
    // Configured in clear off loopback: wherever it goes in clear, the address is the fault.
    assert_eq!(
        kind("http://dav.example.com/", "http://dav.example.com/x"),
        Err(Some(Insecure))
    );
    assert_eq!(
        kind("http://dav.example.com/", "http://127.0.0.1:9/"),
        Err(Some(Insecure))
    );
    assert_eq!(
        kind("http://dav.example.com/", "http://p1.example.net/"),
        Err(Some(Insecure))
    );
    // Configured on TLS, led to plaintext, loopback included.
    assert_eq!(
        kind("https://dav.example.com/", "http://dav.example.com/"),
        Err(Some(LedToInsecure))
    );
    assert_eq!(
        kind("https://dav.example.com/", "http://127.0.0.1:9/"),
        Err(Some(LedToInsecure))
    );
    // Configured on plaintext loopback, led off it.
    assert_eq!(
        kind("http://127.0.0.1:9/", "http://dav.example.com/"),
        Err(Some(LedToInsecure))
    );
    assert_eq!(
        kind("http://[::1]:9/", "http://dav.example.com/"),
        Err(Some(LedToInsecure))
    );
}
