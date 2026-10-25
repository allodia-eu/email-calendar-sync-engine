//! A host's credential policy over the live transport: discovery may lead the connection to
//! another origin, and the credential goes there only when the policy permits that host.

use std::sync::Arc;

use super::CredentialHosts;
use crate::{
    CalDavConfig, CalDavProvider, CardDavConfig, CardDavProvider, Credentials,
    test_support::{mock_server_capturing, multistatus, redirect},
    transport::{DavClient, DavExecutor},
};

const EMPTY: &str = r#"<d:multistatus xmlns:d="DAV:"/>"#;

/// Permits exactly one host.
struct Only(&'static str);

impl CredentialHosts for Only {
    fn permits(&self, host: &str) -> bool {
        host == self.0
    }
}

fn token() -> Credentials {
    Credentials::Bearer("access-token".to_owned())
}

/// The port of a mock server's `http://127.0.0.1:<port>` origin.
fn port(origin: &str) -> &str {
    origin.rsplit(':').next().expect("a port")
}

#[tokio::test]
async fn trust_origin_refuses_a_host_the_policy_does_not_permit() {
    let (base, _) = mock_server_capturing(vec![]);
    let client = DavClient::new(
        &base,
        token(),
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
    .with_credential_hosts(Some(Arc::new(Only("127.0.0.1"))));

    assert!(client.trust_origin("http://127.0.0.1:1/principals/"));
    assert!(
        !client.trust_origin("http://localhost:1/principals/"),
        "a host outside the policy must not receive the credential"
    );
}

#[tokio::test]
async fn a_caldav_redirect_onto_a_refused_host_fails_the_connect_and_sends_it_nothing() {
    // The account's server redirects discovery to another host, as a well-known commonly does.
    // `localhost` and `127.0.0.1` are the same machine and different hosts, which is all a
    // host-name policy can see.
    // The moved host answers, so a connect that went there would be seen.
    let (moved, moved_seen) = mock_server_capturing(vec![multistatus(EMPTY); 4]);
    let moved = format!("http://localhost:{}/dav/", port(&moved));
    let (base, _) = mock_server_capturing(vec![redirect(&moved)]);
    let config = CalDavConfig::new(base, token(), engine_http::test_server::user_agent())
        .with_credential_hosts(Arc::new(Only("127.0.0.1")));

    assert!(CalDavProvider::connect(config).await.is_err());
    assert!(
        moved_seen.lock().expect("seen").is_empty(),
        "the refused host was contacted"
    );
}

#[tokio::test]
async fn a_carddav_redirect_onto_a_refused_host_fails_the_connect_and_sends_it_nothing() {
    // The moved host answers, so a connect that went there would be seen.
    let (moved, moved_seen) = mock_server_capturing(vec![multistatus(EMPTY); 4]);
    let moved = format!("http://localhost:{}/dav/", port(&moved));
    let (base, _) = mock_server_capturing(vec![redirect(&moved)]);
    let config = CardDavConfig::new(base, token(), engine_http::test_server::user_agent())
        .with_credential_hosts(Arc::new(Only("127.0.0.1")));

    assert!(CardDavProvider::connect(config).await.is_err());
    assert!(moved_seen.lock().expect("seen").is_empty());
}

#[tokio::test]
async fn a_base_url_on_a_refused_host_is_never_dialled() {
    let (base, seen) = mock_server_capturing(vec![multistatus(EMPTY); 4]);
    let base = format!("http://localhost:{}", port(&base));
    let config = CalDavConfig::new(base, token(), engine_http::test_server::user_agent())
        .with_credential_hosts(Arc::new(Only("127.0.0.1")));

    assert!(CalDavProvider::connect(config).await.is_err());
    assert!(seen.lock().expect("seen").is_empty());
}

#[tokio::test]
async fn without_a_policy_any_origin_the_server_leads_to_is_trusted() {
    let (base, _) = mock_server_capturing(vec![]);
    let client = DavClient::new(
        &base,
        token(),
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    assert!(client.trust_origin("http://localhost:1/principals/"));
}
