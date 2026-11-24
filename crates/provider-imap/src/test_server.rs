//! TEST BUILDS ONLY: an in-process IMAP server over TLS that signs anyone in and says it is
//! Gmail (`X-GM-EXT-1`), for a suite that needs a real [`ImapAccount`](crate::ImapAccount)
//! and its [`GmailMail`](crate::GmailMail) without a network.
//!
//! Behind the `test-server` feature, which only dev-dependencies turn on, so the Gmail adapter's
//! suite connects through the real `ImapAccount::connect` rather than keeping a fake of its own.

use std::sync::Arc;

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    },
};

/// Starts a server on the loopback address that answers one connection: a greeting, `OK` to
/// whatever signs in, and a `CAPABILITY` naming `X-GM-EXT-1`. Returns the self-signed
/// certificate it presents (for the name `127.0.0.1`) and its address, `127.0.0.1:<port>`.
///
/// # Panics
///
/// When the certificate cannot be made or the port cannot be bound, as a test helper does.
pub async fn gmail() -> (CertificateDer<'static>, String) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed cert");
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
    let config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(tokio_rustls::rustls::DEFAULT_VERSIONS)
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.into())
    .expect("server cert/key");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let Ok((tcp, _)) = listener.accept().await else {
            return;
        };
        let Ok(tls) = acceptor.accept(tcp).await else {
            return;
        };
        let mut stream = BufReader::new(tls);
        let _ = stream
            .write_all(b"* OK [CAPABILITY IMAP4rev1] ready\r\n")
            .await;
        loop {
            let mut line = String::new();
            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let tag = line.split_whitespace().next().unwrap_or("*").to_owned();
            if line.to_ascii_uppercase().contains("CAPABILITY") {
                let _ = stream
                    .write_all(b"* CAPABILITY IMAP4rev1 X-GM-EXT-1\r\n")
                    .await;
            }
            if stream
                .write_all(format!("{tag} OK done\r\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    });
    (cert, format!("127.0.0.1:{}", addr.port()))
}
