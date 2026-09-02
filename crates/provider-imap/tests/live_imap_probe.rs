//! Gated live integration: the pre-authentication **auth probe** against the Stalwart
//! harness, on all four secured transports (IMAP 993/143, submission 465/587).
//!
//! What only a live server can show is that the answer survives the dial. The offline
//! suite pins the classification against captured capability lines; it cannot show that
//! a STARTTLS probe reads the post-upgrade capability rather than the cleartext one, or
//! that the `EHLO` a probe sends gets the same `AUTH` line a submission would — and both
//! of those decide what a setup screen offers.
//!
//! Trusts the harness's self-signed cert via a test-only no-verify verifier, never a
//! host trust store. Skips with no `STALWART_HTTP_ADDR`, so the offline
//! `cargo test --workspace` stays green with no Docker.

use provider_imap::{AuthOffer, ImapSecurity, probe_imap_auth, probe_smtp_auth};
use stalwart_harness::Harness;
use tokio_rustls::TlsConnector;

/// A TLS connector that accepts the harness's self-signed certificate. Test-only and
/// deliberately insecure; it never touches a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

/// The host portion of a `host:port` address (the TLS SNI/cert name).
fn host_of(addr: &str) -> &str {
    addr.rsplit_once(':').map_or("localhost", |(host, _)| host)
}

/// Whether `offer` names `mechanism`, case-insensitively.
fn offers(offer: &AuthOffer, mechanism: &str) -> bool {
    offer
        .mechanisms
        .iter()
        .any(|name| name.eq_ignore_ascii_case(mechanism))
}

#[tokio::test]
async fn the_imap_probe_reads_the_harness_capability_line_on_both_transports() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping: STALWART_HTTP_ADDR unset");
        return;
    };
    let connector = no_verify_connector();

    for (label, addr, security) in [
        (
            "implicit TLS",
            &harness.imap_addr,
            ImapSecurity::ImplicitTls,
        ),
        (
            "STARTTLS",
            &harness.imap_starttls_addr,
            ImapSecurity::StartTls,
        ),
    ] {
        let offer = probe_imap_auth(addr, host_of(addr), security, &connector)
            .await
            .unwrap_or_else(|err| panic!("probe {label}: {err}"));

        // Stalwart offers all three. The STARTTLS row is the one that matters: its
        // cleartext capability line is a different, shorter list, so reading the offer
        // before the upgrade would answer a question about a session nobody will use.
        assert!(
            offer.oauth,
            "{label}: expected an OAuth mechanism: {offer:?}"
        );
        assert!(offer.password, "{label}: expected a password: {offer:?}");
        assert!(offers(&offer, "OAUTHBEARER"), "{label}: {offer:?}");
        assert!(offers(&offer, "XOAUTH2"), "{label}: {offer:?}");
        assert!(offers(&offer, "PLAIN"), "{label}: {offer:?}");
    }
}

#[tokio::test]
async fn the_smtp_probe_reads_the_submission_auth_line_on_both_transports() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping: STALWART_HTTP_ADDR unset");
        return;
    };
    let connector = no_verify_connector();
    // The domain a real submission from this account would introduce itself with.
    let ehlo = harness
        .account
        .rsplit_once('@')
        .map_or("localhost", |(_, domain)| domain);

    for (label, addr, security) in [
        (
            "implicit TLS",
            &harness.smtp_tls_addr,
            ImapSecurity::ImplicitTls,
        ),
        (
            "STARTTLS",
            &harness.smtp_starttls_addr,
            ImapSecurity::StartTls,
        ),
    ] {
        let offer = probe_smtp_auth(addr, host_of(addr), security, ehlo, &connector)
            .await
            .unwrap_or_else(|err| panic!("probe {label}: {err}"));

        assert!(
            offer.oauth,
            "{label}: expected an OAuth mechanism: {offer:?}"
        );
        assert!(offer.password, "{label}: expected a password: {offer:?}");
    }
}

#[tokio::test]
async fn a_probe_never_leaves_the_server_unable_to_take_the_next_connection() {
    // The probe closes each dial with `LOGOUT`/`QUIT` rather than dropping the socket,
    // and a setup screen may run several in a row (retyped address, corrected host).
    // Ten back-to-back probes against one server is the cheap way to notice a probe that
    // leaks a session: Stalwart caps concurrent connections per account, so a leaked one
    // shows up here as a refused dial rather than as a slow leak nobody sees.
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping: STALWART_HTTP_ADDR unset");
        return;
    };
    let connector = no_verify_connector();
    for attempt in 0..10 {
        probe_imap_auth(
            &harness.imap_addr,
            host_of(&harness.imap_addr),
            ImapSecurity::ImplicitTls,
            &connector,
        )
        .await
        .unwrap_or_else(|err| panic!("probe {attempt}: {err}"));
    }
}
