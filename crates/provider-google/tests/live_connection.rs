//! Gated live check of the transport facts a Google connection reports: the
//! negotiated **TLS version** and HTTP version that `Provider::connection_info()`
//! publishes (`docs/agent-guidance/tls.md`).
//!
//! Its own file because it asserts a property of the *connection*, not of mail sync —
//! and because it is the only place the TLS half can be verified against Google
//! itself. The offline suites cannot: their mock servers are cleartext, and reqwest's
//! `TlsInfo` has private fields, so no fake can construct one. A real handshake is the
//! only thing that fills it in.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set (an OAuth bearer access token, e.g. from
//! `tools/google-oauth`), so the offline `cargo test --workspace` stays green. Run it
//! locally:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_connection -- --nocapture
//! ```

use engine_core::ids::AccountId;
use engine_provider::Provider;
use provider_google::{GmailProvider, GoogleClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GmailProvider::new(client)
}

#[tokio::test]
async fn live_the_connection_reports_the_negotiated_tls_version() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_the_connection_reports_the_negotiated_tls_version: GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = provider(token);
    // Google runs no session discovery, so `connect` issued nothing and there is nothing
    // to report yet. Asserting this first is what makes the next assertion evidence: it
    // proves the version below came from *this* exchange rather than a constant.
    assert_eq!(provider.connection_info().tls_version, None);

    provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync labels");

    // One real request to `gmail.googleapis.com` over the engine's own TLS stack. The
    // offline suites cannot produce this: the mock servers are cleartext and reqwest's
    // `TlsInfo` has private fields, so only a real handshake fills it in.
    let info = provider.connection_info();
    assert_eq!(
        info.tls_version,
        Some(engine_provider::TlsVersion::Tls1_3),
        "Google negotiates TLS 1.3; a `None` here means the shared client stopped asking \
         for reqwest's TlsInfo extension"
    );
    // Google speaks HTTP/2, so the same response carried both facts — which is the reason
    // they are recorded together.
    assert_eq!(info.http_version, Some(engine_provider::HttpVersion::Http2));
}
