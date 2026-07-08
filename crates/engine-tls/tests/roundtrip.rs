//! In-process TLS round-trip: proves one [`TlsPolicy`] governs **both** transports
//! — the `reqwest` client (CalDAV/JMAP/Graph) and the `tokio-rustls` connector
//! (IMAP/SMTP) — accepting a trusted certificate and rejecting an untrusted one.
//!
//! The Stalwart harness serves plaintext HTTP, so the live suite cannot exercise
//! the reqwest TLS verification path; this in-process server is the authoritative
//! check. Runs only with the `reqwest` feature (CI's `--all-features`).
#![cfg(feature = "reqwest")]

use std::sync::Arc;

use engine_tls::{CertificateDer, TlsClientConfig, TlsPolicy, client_config};
use rustls::pki_types::{PrivatePkcs8KeyDer, ServerName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsAcceptor;

/// Starts a TLS server (valid for `127.0.0.1`) that answers one minimal HTTP/1.1
/// `200` per connection. Returns the server's certificate and bound port.
async fn tls_server() -> (CertificateDer<'static>, u16) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed cert");
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());

    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
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
                // A failed client handshake (untrusted cert) surfaces here as an
                // error; just drop that connection.
                if let Ok(mut tls) = acceptor.accept(tcp).await {
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    (cert, port)
}

#[tokio::test]
async fn reqwest_client_trusts_pinned_and_rejects_untrusted() {
    let (cert, port) = tls_server().await;
    let url = format!("https://127.0.0.1:{port}/");

    // Pinned to the server's own cert → handshake + GET succeed.
    let trusted = client_config(&TlsPolicy::pinned(vec![cert.clone()])).unwrap();
    let ok = trusted
        .reqwest_builder()
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await;
    assert!(ok.is_ok(), "pinned roots should trust the server: {ok:?}");
    assert_eq!(ok.unwrap().status().as_u16(), 200);

    // Bundled Mozilla roots do not include the self-signed cert → rejected.
    let rejected = TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await;
    assert!(
        rejected.is_err(),
        "bundled roots must reject an untrusted cert"
    );

    // Firefox-style union (bundled ∪ custom) trusts it again.
    let union = client_config(&TlsPolicy::roots(true, false, vec![cert])).unwrap();
    let ok = union
        .reqwest_builder()
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await;
    assert!(ok.is_ok(), "bundled+custom union should trust: {ok:?}");
}

#[tokio::test]
async fn connector_trusts_pinned_and_rejects_untrusted() {
    let (cert, port) = tls_server().await;
    let name = ServerName::try_from("127.0.0.1").unwrap();

    // The IMAP/SMTP connector handshakes when pinned to the server's cert.
    let trusted = client_config(&TlsPolicy::pinned(vec![cert])).unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert!(
        trusted.connector().connect(name.clone(), tcp).await.is_ok(),
        "pinned connector should handshake"
    );

    // Bundled roots reject the self-signed cert.
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert!(
        TlsClientConfig::bundled()
            .connector()
            .connect(name, tcp)
            .await
            .is_err(),
        "bundled connector must reject an untrusted cert"
    );
}

/// The test-only accept-any config handshakes with the self-signed cert that
/// bundled roots reject — the path the gated IMAP live tests rely on.
#[cfg(feature = "dangerous-testing")]
#[tokio::test]
async fn dangerous_accept_any_trusts_untrusted() {
    let (_cert, port) = tls_server().await;
    let name = ServerName::try_from("127.0.0.1").unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert!(
        TlsClientConfig::dangerous_accept_any()
            .connector()
            .connect(name, tcp)
            .await
            .is_ok(),
        "accept-any connector should handshake with a self-signed cert"
    );
}
