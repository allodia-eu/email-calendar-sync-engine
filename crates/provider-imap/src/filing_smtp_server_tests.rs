//! Offline coverage of the SMTP submission dispatch ([`ImapProvider::submit`]) over an
//! in-process SMTP server, for the two TLS transports whose real socket + TLS dial the
//! `MockStream` cannot reach: implicit TLS and post-`STARTTLS`.
//!
//! The mock transcripts in `smtp_starttls_tests` already assert the wire *shape* of the
//! STARTTLS preamble and the post-upgrade conversation; these instead exercise the
//! provider-side dispatch — `submit` picking a transport, dialing it, TLS-wrapping the
//! socket, and returning a receipt. The IMAP Sent-filing that follows a delivered send
//! degrades gracefully over the exhausted mock connection (best-effort placement), so it
//! is isolated from what these assert.

use std::sync::Arc;

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::Draft;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer},
};

use super::{SmtpSender, resolve_smtp};
use crate::{
    ImapProvider,
    config::ImapConfig,
    mock::{MockStream, script},
    transport::Connection,
};

/// A self-signed cert and the TLS acceptor presenting it — the server half of the trust
/// pair (the client trusts `cert` via [`trusting_connector`]).
fn cert_and_acceptor() -> (engine_tls::CertificateDer<'static>, TlsAcceptor) {
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
    (cert, TlsAcceptor::from(Arc::new(config)))
}

/// A connector trusting only `cert` — the host-injected trust the library never bakes in
/// (`docs/agent-guidance/tls.md`).
fn trusting_connector(cert: engine_tls::CertificateDer<'static>) -> TlsConnector {
    engine_tls::client_config(&engine_tls::TlsPolicy::pinned(vec![cert]))
        .expect("client config")
        .connector()
}

/// Serves the SMTP submission conversation a delivered send drives over an
/// already-secured `stream`, **without** a greeting (implicit TLS writes its greeting
/// before calling this; STARTTLS sends none post-upgrade): `EHLO → AUTH → MAIL → RCPT →
/// DATA` (consuming the message to its `.` terminator) → `250` → `QUIT`. Accepts every
/// command so the transport, not the server's policy, is what the test exercises.
async fn serve_delivery<S>(stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if buf.read_line(&mut line).await.expect("read command") == 0 {
            return;
        }
        let upper = line.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            b"250-mail\r\n250 AUTH PLAIN\r\n"
        } else if upper.starts_with("AUTH") {
            b"235 2.7.0 Authentication successful\r\n"
        } else if upper.starts_with("MAIL") || upper.starts_with("RCPT") {
            b"250 2.1.0 OK\r\n"
        } else if upper.starts_with("DATA") {
            buf.write_all(b"354 go ahead\r\n")
                .await
                .expect("data ready");
            buf.flush().await.expect("flush data ready");
            loop {
                let mut data = String::new();
                let read = buf.read_line(&mut data).await.expect("read data");
                if read == 0 || data == ".\r\n" {
                    break;
                }
            }
            b"250 2.0.0 queued\r\n"
        } else if upper.starts_with("QUIT") {
            let _ = buf.write_all(b"221 2.0.0 bye\r\n").await;
            return;
        } else {
            continue;
        };
        buf.write_all(reply).await.expect("reply");
        buf.flush().await.expect("flush reply");
    }
}

/// An implicit-TLS SMTP server: TLS from the first byte, then greeting + [`serve_delivery`].
async fn implicit_tls_server() -> (engine_tls::CertificateDer<'static>, u16) {
    let (cert, acceptor) = cert_and_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut tls = acceptor.accept(tcp).await.expect("handshake");
        tls.write_all(b"220 mail ESMTP ready\r\n")
            .await
            .expect("greeting");
        tls.flush().await.expect("flush greeting");
        serve_delivery(tls).await;
    });
    (cert, port)
}

/// A STARTTLS submission server: plaintext greeting + `EHLO` (advertising `STARTTLS`) +
/// `STARTTLS`, then upgrades the socket and runs the greeting-less [`serve_delivery`].
async fn starttls_server() -> (engine_tls::CertificateDer<'static>, u16) {
    let (cert, acceptor) = cert_and_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut plain = BufReader::new(tcp);
        plain
            .write_all(b"220 mail ESMTP ready\r\n")
            .await
            .expect("greeting");
        plain.flush().await.expect("flush greeting");
        for reply in [
            "250-mail\r\n250-STARTTLS\r\n250 AUTH PLAIN\r\n",
            "220 2.0.0 Ready to start TLS\r\n",
        ] {
            let mut line = String::new();
            plain.read_line(&mut line).await.expect("preamble command");
            plain
                .write_all(reply.as_bytes())
                .await
                .expect("preamble reply");
            plain.flush().await.expect("flush reply");
        }
        // Client sends nothing between the STARTTLS 220 and its ClientHello, so the raw
        // socket unwraps clean.
        let tcp = plain.into_inner();
        let tls = acceptor.accept(tcp).await.expect("handshake");
        serve_delivery(tls).await;
    });
    (cert, port)
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("submit-server@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body text",
    )
}

/// A provider whose IMAP side is an empty mock (so the post-send Sent filing fails
/// gracefully, isolating the SMTP transport) and whose SMTP sender is `sender`.
fn provider_with_smtp(sender: SmtpSender) -> ImapProvider<MockStream> {
    let (stream, _) = MockStream::new(script(&[]));
    ImapProvider::with_connection_and_smtp(
        Connection::resume(stream),
        engine_core::ids::MailboxId::try_from("INBOX").unwrap(),
        sender,
    )
}

#[tokio::test]
async fn submit_over_implicit_tls_dials_wraps_and_delivers() {
    let (cert, port) = implicit_tls_server().await;
    let config = ImapConfig::new("h:993", "127.0.0.1", "alice", "pw")
        .with_smtp_tls(format!("127.0.0.1:{port}"), "127.0.0.1");
    let sender = resolve_smtp(
        config.smtp.as_ref().unwrap(),
        &trusting_connector(cert),
        &config,
    );

    let receipt = provider_with_smtp(sender)
        .submit(&draft())
        .await
        .expect("implicit-TLS submit delivers");
    assert_eq!(receipt.message_id, draft().message_id);
}

#[tokio::test]
async fn submit_over_starttls_negotiates_upgrades_and_delivers() {
    let (cert, port) = starttls_server().await;
    let config = ImapConfig::new("h:993", "127.0.0.1", "alice", "pw")
        .with_smtp_starttls(format!("127.0.0.1:{port}"), "127.0.0.1");
    let sender = resolve_smtp(
        config.smtp.as_ref().unwrap(),
        &trusting_connector(cert),
        &config,
    );

    let receipt = provider_with_smtp(sender)
        .submit(&draft())
        .await
        .expect("STARTTLS submit delivers");
    assert_eq!(receipt.message_id, draft().message_id);
}
