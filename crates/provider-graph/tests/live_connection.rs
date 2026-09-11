//! Gated live check of the transport facts a Microsoft Graph connection reports: the
//! negotiated **TLS version** and HTTP version that `Provider::connection_info()`
//! publishes (`docs/agent-guidance/tls.md`).
//!
//! Its own file because it asserts a property of the *connection*, not of mail sync —
//! and because it is the only place the TLS half can be verified against Microsoft Graph
//! itself. The offline suites cannot: their mock servers are cleartext, and reqwest's
//! `TlsInfo` has private fields, so no fake can construct one. A real handshake is the
//! only thing that fills it in.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set (an OAuth bearer access token, e.g. from
//! `tools/graph-oauth`), so the offline `cargo test --workspace` stays green. Run it
//! locally:
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-graph --test live_connection -- --nocapture
//! ```

use engine_core::ids::{AccountId, MailboxId};
use engine_provider::Provider;
use provider_graph::{GraphClient, GraphProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// A provider bound to the inbox (Graph accepts the well-known alias in the URL).
fn provider(token: String) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

#[tokio::test]
async fn live_the_connection_reports_the_negotiated_tls_version() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_the_connection_reports_the_negotiated_tls_version: GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let provider = provider(token);
    // Graph runs no session discovery, so `connect` issued nothing and there is nothing
    // to report yet. Asserting this first is what makes the next assertion evidence: it
    // proves the version below came from *this* exchange rather than a constant.
    assert_eq!(provider.connection_info().tls_version, None);

    provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");

    // One real request to `graph.microsoft.com` over the engine's own TLS stack. The
    // offline suites cannot produce this: the mock servers are cleartext and reqwest's
    // `TlsInfo` has private fields, so only a real handshake fills it in.
    let info = provider.connection_info();
    assert_eq!(
        info.tls_version,
        Some(engine_provider::TlsVersion::Tls1_3),
        "Graph negotiates TLS 1.3; a `None` here means the shared client stopped asking \
         for reqwest's TlsInfo extension"
    );
    // The same response carried both facts — which is the reason they are recorded
    // together. Observed: `graph.microsoft.com` answers **HTTP/1.1** even though the
    // shared client offers `h2` first, and an independent `curl --http2` agrees, so this
    // is Microsoft's choice and not a missing ALPN offer. (Google, on the same client,
    // answers HTTP/2 — see `provider-google`'s counterpart.)
    assert_eq!(
        info.http_version,
        Some(engine_provider::HttpVersion::Http1_1)
    );
}
