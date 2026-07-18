//! Offline coverage of the IMAP `STARTTLS` connect path
//! ([`connect_session`](crate::provider::connect_session)) against an in-process TLS
//! server, mirroring `tls_info`'s implicit-TLS harness.
//!
//! `MockStream` bypasses TLS, so the STARTTLS arm's real sequence — plaintext preamble
//! → socket upgrade → greeting-less resumed `LOGIN` — is only reachable against a socket
//! that actually completes a handshake. This stands one up with a self-signed cert the
//! provider is told to trust (`docs/agent-guidance/tls.md`: the host injects trust), so
//! the upgrade is exercised offline instead of only in the gated live test.

use std::sync::Arc;

use engine_core::ids::MailboxId;
use engine_provider::{Provider, TlsVersion};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    TlsAcceptor,
    client::TlsStream,
    rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer},
};

use crate::{ImapConfig, ImapProvider};

/// Serves one IMAP `STARTTLS` connect: the plaintext greeting and `CAPABILITY` (both
/// advertising `STARTTLS`) plus the `STARTTLS` command, then upgrades the raw socket and
/// speaks the resumed `LOGIN` + `CAPABILITY` over TLS. Returns its self-signed cert and
/// bound port. The plaintext side strictly answers one command at a time, so the client
/// never buffers past the `STARTTLS` OK before its own `ClientHello`.
async fn imap_starttls_server() -> (engine_tls::CertificateDer<'static>, u16) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed cert");
    let cert = generated.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());

    let server_config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(tokio_rustls::rustls::DEFAULT_VERSIONS)
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.into())
    .expect("server cert/key");
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        // Plaintext preamble: greeting, then answer `CAPABILITY` (a1) and `STARTTLS` (a2).
        let mut plain = BufReader::new(tcp);
        plain
            .write_all(b"* OK [CAPABILITY IMAP4rev1 STARTTLS] ready\r\n")
            .await
            .expect("greeting");
        plain.flush().await.expect("flush greeting");
        for reply in [
            "* CAPABILITY IMAP4rev1 STARTTLS\r\na1 OK CAPABILITY done\r\n",
            "a2 OK Begin TLS negotiation now\r\n",
        ] {
            let mut line = String::new();
            plain.read_line(&mut line).await.expect("preamble command");
            plain
                .write_all(reply.as_bytes())
                .await
                .expect("preamble reply");
            plain.flush().await.expect("flush reply");
        }
        // Upgrade the raw socket; the resumed session restarts tags at 1.
        let tcp = plain.into_inner();
        let tls = acceptor.accept(tcp).await.expect("handshake");
        let mut stream = BufReader::new(tls);
        for reply in ["OK LOGIN completed", "OK CAPABILITY completed"] {
            let mut line = String::new();
            stream.read_line(&mut line).await.expect("command");
            let tag = line.split_whitespace().next().expect("tag").to_owned();
            if line.contains("CAPABILITY") {
                stream
                    .write_all(b"* CAPABILITY IMAP4rev1 IDLE\r\n")
                    .await
                    .expect("untagged capability");
            }
            stream
                .write_all(format!("{tag} {reply}\r\n").as_bytes())
                .await
                .expect("tagged completion");
            stream.flush().await.expect("flush completion");
        }
    });
    (cert, port)
}

#[tokio::test]
async fn starttls_connect_upgrades_and_reports_the_negotiated_version() {
    let (cert, port) = imap_starttls_server().await;
    let tls = engine_tls::client_config(&engine_tls::TlsPolicy::pinned(vec![cert]))
        .expect("client config");
    let config =
        ImapConfig::new(format!("127.0.0.1:{port}"), "127.0.0.1", "alice", "pw").with_starttls();
    let provider: ImapProvider<TlsStream<TcpStream>> = ImapProvider::connect(
        &config,
        tls.connector(),
        MailboxId::try_from("INBOX").expect("mailbox"),
    )
    .await
    .expect("STARTTLS connect");

    let info = provider.connection_info();
    // The version is read off the *post-upgrade* handshake, proving the socket really
    // became TLS after the plaintext STARTTLS preamble (not implicit TLS from byte one).
    assert_eq!(info.tls_version, Some(TlsVersion::Tls1_3));
    // The post-auth CAPABILITY (over TLS) still drives the reported capabilities.
    assert!(info.capabilities.mail() && info.capabilities.idle());
}
