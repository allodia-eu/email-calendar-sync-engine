//! In-process TLS round-trip for [`ObservedConnection`]: proves an HTTP adapter really
//! learns the negotiated TLS version from a response, over the same client the four
//! providers build.
//!
//! The unit tests in `observed.rs` cover the version mapping, but they cannot cover the
//! step that actually breaks — reading reqwest's `TlsInfo` extension off a live
//! response — because the extension's fields are private and no fake can construct one.
//! Only a real handshake produces it, so the boundary is driven here rather than left
//! to the live suites (`AGENTS.md`).

use std::sync::Arc;

use engine_http::ObservedConnection;
use engine_provider::{HttpVersion, TlsVersion};
use engine_tls::{CertificateDer, TlsPolicy, client_config};
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::TlsAcceptor;

/// The minimal HTTP/1.1 `200` the version tests are served.
const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// Starts a TLS server (valid for `127.0.0.1`) answering one minimal HTTP/1.1 `200`
/// per connection, pinned to `versions`. Returns its certificate and bound port.
async fn tls_server(
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> (CertificateDer<'static>, u16) {
    tls_server_replying(versions, OK).await
}

/// The same, answering `reply` verbatim — so a test can serve the refusal a
/// `ThrottleClassifier` has to read.
async fn tls_server_replying(
    versions: &[&'static rustls::SupportedProtocolVersion],
    reply: &'static [u8],
) -> (CertificateDer<'static>, u16) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed cert");
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());

    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(versions)
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.into())
    .expect("server cert/key");
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(tcp).await {
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls.write_all(reply).await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    (cert, port)
}

/// `GET`s `port` over the shared client and records the response.
async fn observe(cert: CertificateDer<'static>, port: u16) -> ObservedConnection {
    let response = client_config(&TlsPolicy::pinned(vec![cert]))
        .expect("client config")
        .reqwest_builder()
        .build()
        .expect("client")
        .get(format!("https://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("GET over TLS should succeed");
    let observed = ObservedConnection::default();
    observed.record(&response);
    observed
}

/// One response carries **both** transport facts, which is the whole reason they are
/// recorded together: a transport that observed the HTTP version already held the TLS
/// version in its hand.
#[tokio::test]
async fn one_response_yields_both_transport_facts() {
    let (cert, port) = tls_server(rustls::DEFAULT_VERSIONS).await;
    let observed = observe(cert, port).await;
    assert_eq!(observed.tls_version(), Some(TlsVersion::Tls1_3));
    assert_eq!(observed.http_version(), Some(HttpVersion::Http1_1));
}

/// The version is read from the handshake, not assumed: a 1.2-only server is reported
/// as TLS 1.2.
#[tokio::test]
async fn a_tls_1_2_server_is_reported_as_tls_1_2() {
    let (cert, port) = tls_server(&[&rustls::version::TLS12]).await;
    assert_eq!(
        observe(cert, port).await.tls_version(),
        Some(TlsVersion::Tls1_2)
    );
}

/// A reply a [`ThrottleClassifier`](engine_http::ThrottleClassifier) had to read is taken
/// apart and put back together, and reqwest's `TlsInfo` rides in the response's
/// *extensions* — the one part of a reply that is easy to drop while reassembling it and
/// impossible to notice, because nothing fails: the adapter simply reports no TLS version
/// from then on.
///
/// That would land on exactly the accounts a support log is opened for. So the round trip
/// is driven through the funnel that does the reassembling, on the path that does it — a
/// `403` claimed by a classifier — rather than asserted against a hand-built response, for
/// the same reason the tests above exist: only a real handshake makes a `TlsInfo`.
#[tokio::test]
async fn a_reply_the_classifier_had_to_read_still_reports_its_tls_version() {
    const QUOTA: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 18\r\n\
        Connection: close\r\n\r\n{\"reason\":\"quota\"}";
    const READS: &[u16] = &[403];

    let (cert, port) = tls_server_replying(&[&rustls::version::TLS12], QUOTA).await;
    let client = client_config(&TlsPolicy::pinned(vec![cert]))
        .expect("client config")
        .reqwest_builder()
        .build()
        .expect("client");
    // Claims the `403` and then declines it, so the reply is buffered and rebuilt exactly
    // once and handed straight back — the shortest path through the reassembly.
    let retry = engine_http::RetryConfig::default()
        .labelled("test")
        .classifying(Arc::new((READS, |_: u16, _: &[u8]| None)));
    let response =
        engine_http::send_retrying(client.get(format!("https://127.0.0.1:{port}/")), &retry)
            .await
            .expect("GET over TLS should succeed");

    let observed = ObservedConnection::default();
    observed.record(&response);
    assert_eq!(observed.tls_version(), Some(TlsVersion::Tls1_2));
    assert_eq!(observed.http_version(), Some(HttpVersion::Http1_1));
    assert_eq!(response.status().as_u16(), 403);
}
