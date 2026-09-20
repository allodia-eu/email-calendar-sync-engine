//! Certificate exceptions against a real handshake: a server whose certificate no
//! trust anchor can rescue is refused, its certificate is recorded, and an exception
//! naming that exact certificate — and nothing else — lets the same server through.
//!
//! The server here serves a **self-signed CA certificate as its own end-entity
//! certificate**, which is what Proton Mail Bridge's local IMAP/SMTP listener does and
//! what `CaUsedAsEndEntity` names. The first test is the one that decides the design:
//! the webpki verifier reads the leaf's own basic constraints before it looks at a
//! trust anchor, so adding that certificate as a root does **not** make the server
//! reachable, and an exception is the only mechanism that does.

use std::sync::Arc;

use engine_tls::{
    CertificateDer, CertificateException, TlsPolicy, client_config, client_config_with_exceptions,
};
use rustls::pki_types::{PrivatePkcs8KeyDer, ServerName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsAcceptor;

/// A self-signed certificate for `127.0.0.1` that is marked `CA:TRUE` — a certificate
/// authority, served as the leaf. Returns it with its key.
fn ca_shaped_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
    let key = rcgen::KeyPair::generate().expect("key pair");
    let mut params =
        rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).expect("certificate params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key).expect("self-signed certificate");
    (
        cert.der().clone(),
        PrivatePkcs8KeyDer::from(key.serialize_der()),
    )
}

/// Starts a TLS server presenting `cert`, answering one byte per connection so a
/// completed handshake is observable. Returns the bound port.
async fn tls_server(cert: CertificateDer<'static>, key: PrivatePkcs8KeyDer<'static>) -> u16 {
    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert], key.into())
    .expect("server cert/key");
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                // A refused client handshake surfaces here as an error; drop it.
                if let Ok(mut tls) = acceptor.accept(tcp).await {
                    let mut buf = [0u8; 64];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls.write_all(b".").await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    port
}

fn name() -> ServerName<'static> {
    ServerName::try_from("127.0.0.1").expect("server name")
}

/// The case the whole feature exists for: a CA certificate served as the leaf is
/// refused **even when it is a trust anchor**, because the leaf's own basic
/// constraints decide that before any anchor is consulted. Adding it as a root is not
/// a fix, so the exception is not a convenience for one.
#[tokio::test]
async fn a_ca_certificate_served_as_the_leaf_is_refused_even_as_a_trust_anchor() {
    let (cert, key) = ca_shaped_cert();
    let port = tls_server(cert.clone(), key).await;

    let pinned = client_config(&TlsPolicy::pinned(vec![cert])).expect("config");
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("tcp");
    let error = pinned
        .connector()
        .connect(name(), tcp)
        .await
        .expect_err("a CA certificate used as an end entity is not made valid by trusting it");
    assert!(
        error.to_string().contains("CaUsedAsEndEntity"),
        "expected the leaf's basic constraints to decide it, got: {error}"
    );
}

/// The refusal names what it refused, so a client has something to show: the server
/// name it asked for and the certificate that server presented.
#[tokio::test]
async fn a_refusal_records_the_certificate_the_server_presented() {
    let (cert, key) = ca_shaped_cert();
    let port = tls_server(cert.clone(), key).await;

    let config = client_config(&TlsPolicy::bundled()).expect("config");
    assert!(config.rejected().is_none(), "nothing refused yet");

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("tcp");
    let _ = config
        .connector()
        .connect(name(), tcp)
        .await
        .expect_err("refused");

    let rejected = config.rejected().expect("the refusal was recorded");
    assert_eq!(rejected.server_name(), "127.0.0.1");
    assert_eq!(rejected.certificate(), &cert);
    assert_eq!(rejected.fingerprint(), engine_tls::fingerprint(&cert));
}

/// The exception built from that refusal lets the same server through, over both
/// transports, while the bundled roots it is layered on keep refusing everything else.
#[tokio::test]
async fn an_exception_admits_the_server_it_names() {
    let (cert, key) = ca_shaped_cert();
    let port = tls_server(cert.clone(), key).await;

    let exception = CertificateException::new("127.0.0.1", &cert);
    let config = client_config_with_exceptions(&TlsPolicy::bundled(), &[exception])
        .expect("config with an exception");

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("tcp");
    config
        .connector()
        .connect(name(), tcp)
        .await
        .expect("the excepted certificate is accepted");
    assert!(
        config.rejected().is_none(),
        "an accepted handshake records no refusal"
    );
}

/// An exception is worth nothing to anything but the certificate it names: the same
/// server name presenting a *different* certificate is refused exactly as it was.
/// This is what separates an exception from turning verification off.
#[tokio::test]
async fn an_exception_does_not_cover_a_second_certificate_for_the_same_name() {
    let (excepted, _) = ca_shaped_cert();
    let (served, key) = ca_shaped_cert();
    let port = tls_server(served.clone(), key).await;

    let config = client_config_with_exceptions(
        &TlsPolicy::bundled(),
        &[CertificateException::new("127.0.0.1", &excepted)],
    )
    .expect("config with an exception");

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("tcp");
    let _ = config
        .connector()
        .connect(name(), tcp)
        .await
        .expect_err("a certificate nobody excepted is still refused");
    assert_eq!(
        config
            .rejected()
            .expect("the refusal was recorded")
            .fingerprint(),
        engine_tls::fingerprint(&served)
    );
}

/// An exception is scoped to one server name. The certificate is right; the name it
/// was excepted for is not, so verification stands.
#[tokio::test]
async fn an_exception_does_not_cover_a_second_server() {
    let (cert, key) = ca_shaped_cert();
    let port = tls_server(cert.clone(), key).await;

    let config = client_config_with_exceptions(
        &TlsPolicy::bundled(),
        &[CertificateException::new("mail.example.com", &cert)],
    )
    .expect("config with an exception");

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("tcp");
    let _ = config
        .connector()
        .connect(name(), tcp)
        .await
        .expect_err("an exception for another server does not admit this one");
}

/// A server that verifies normally is unaffected: the exception list is consulted only
/// after the policy's own verifier has refused, so carrying one cannot change what a
/// valid certificate means.
#[tokio::test]
async fn a_valid_certificate_still_verifies_normally_alongside_an_exception() {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed");
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
    let port = tls_server(cert.clone(), key).await;

    let (unrelated, _) = ca_shaped_cert();
    let config = client_config_with_exceptions(
        &TlsPolicy::pinned(vec![cert]),
        &[CertificateException::new("127.0.0.1", &unrelated)],
    )
    .expect("config with an exception");

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("tcp");
    config
        .connector()
        .connect(name(), tcp)
        .await
        .expect("a certificate the policy trusts needs no exception");
    assert!(config.rejected().is_none());
}
