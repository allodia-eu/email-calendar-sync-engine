//! The TLS fact an IMAP session reports once its handshake completes.
//!
//! `provider-imap` is the one adapter that *can* report a negotiated TLS version:
//! it drives `tokio-rustls` directly, so the finished `ClientConnection` names the
//! version. The reqwest adapters cannot (`docs/agent-guidance/tls.md`).
//!
//! Captured at connect and stored on the provider, not read from the live stream
//! later: [`ImapProvider`](crate::ImapProvider) is generic over its stream `S` (the
//! offline tests drive it over an in-memory mock), so only the concrete TLS dial in
//! [`connect_session`](crate::dial::connect_session) can observe it.

use engine_provider::TlsVersion;
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, rustls::ProtocolVersion};

/// Maps rustls' negotiated version onto the engine's neutral [`TlsVersion`].
///
/// Anything but TLS 1.2/1.3 is `None`: the shared config pins a TLS 1.2 floor and
/// rustls implements nothing newer than 1.3 (`docs/agent-guidance/tls.md`), so the
/// other ordinals cannot be negotiated. An unmodeled version is reported as unknown
/// rather than failing a connection that rustls itself accepted.
fn from_rustls(version: ProtocolVersion) -> Option<TlsVersion> {
    match version {
        ProtocolVersion::TLSv1_2 => Some(TlsVersion::Tls1_2),
        ProtocolVersion::TLSv1_3 => Some(TlsVersion::Tls1_3),
        _ => None,
    }
}

/// The TLS version negotiated on an established client stream.
///
/// `None` before the handshake agrees a version — unreachable here, since
/// `TlsConnector::connect` resolves only after the handshake completes.
pub(crate) fn tls_version(stream: &TlsStream<TcpStream>) -> Option<TlsVersion> {
    stream.get_ref().1.protocol_version().and_then(from_rustls)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use engine_core::ids::MailboxId;
    use engine_provider::Provider;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer},
    };

    use super::*;
    use crate::{ImapConfig, ImapProvider};

    #[test]
    fn only_the_two_versions_rustls_can_negotiate_map() {
        assert_eq!(
            from_rustls(ProtocolVersion::TLSv1_2),
            Some(TlsVersion::Tls1_2)
        );
        assert_eq!(
            from_rustls(ProtocolVersion::TLSv1_3),
            Some(TlsVersion::Tls1_3)
        );
        // Below the shared config's TLS 1.2 floor, or not a TLS version at all.
        for unnegotiable in [
            ProtocolVersion::SSLv3,
            ProtocolVersion::TLSv1_0,
            ProtocolVersion::TLSv1_1,
            ProtocolVersion::DTLSv1_2,
        ] {
            assert_eq!(from_rustls(unnegotiable), None);
        }
    }

    /// Serves one TLS connection speaking just enough IMAP for
    /// `connect_session` — greeting, `LOGIN`, post-auth `CAPABILITY` — pinned to
    /// `versions`. Returns its self-signed certificate and bound port.
    ///
    /// The provider's own `MockStream` bypasses TLS entirely, so an in-process TLS
    /// server is the only way to cover the real handshake offline (`AGENTS.md`: drive
    /// the transport boundary rather than leaving it to the live tests).
    async fn imap_tls_server(
        versions: &[&'static tokio_rustls::rustls::SupportedProtocolVersion],
    ) -> (engine_tls::CertificateDer<'static>, u16) {
        let generated = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])
            .expect("self-signed cert");
        let cert = generated.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());

        let server_config = ServerConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
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
            let (tcp, _) = listener.accept().await.expect("accept");
            let tls = acceptor.accept(tcp).await.expect("handshake");
            let mut stream = BufReader::new(tls);
            stream
                .write_all(b"* OK [CAPABILITY IMAP4rev1] ready\r\n")
                .await
                .expect("greeting");
            // `connect_session` issues LOGIN then CAPABILITY. Neither response
            // advertises QRESYNC, so no `ENABLE` follows and the exchange ends here.
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
            }
        });
        (cert, port)
    }

    /// Connects a provider to `port`, trusting only `cert` — the host-injected trust
    /// policy the library never bakes in (`docs/agent-guidance/tls.md`).
    async fn connect(
        cert: engine_tls::CertificateDer<'static>,
        port: u16,
    ) -> ImapProvider<TlsStream<TcpStream>> {
        let tls = engine_tls::client_config(&engine_tls::TlsPolicy::pinned(vec![cert]))
            .expect("client config");
        let config = ImapConfig::new(
            format!("127.0.0.1:{port}"),
            "127.0.0.1",
            crate::credentials::Credentials::password("u", "pw"),
        );
        ImapProvider::connect(
            &config,
            tls.connector(),
            MailboxId::try_from("INBOX").expect("mailbox"),
        )
        .await
        .expect("connect")
    }

    #[tokio::test]
    async fn a_real_handshake_reports_the_negotiated_tls_version() {
        let (cert, port) = imap_tls_server(tokio_rustls::rustls::DEFAULT_VERSIONS).await;
        let info = connect(cert, port).await.connection_info();
        assert_eq!(info.tls_version, Some(TlsVersion::Tls1_3));
        // IMAP is not HTTP: the field is not applicable, not merely unobserved.
        assert_eq!(info.http_version, None);
        // The capabilities still come from the post-auth CAPABILITY response.
        assert!(info.capabilities.mail() && info.capabilities.idle());
    }

    #[tokio::test]
    async fn a_tls_1_2_server_is_reported_as_tls_1_2() {
        // Proves the version is *read from the handshake*, not assumed to be 1.3.
        let (cert, port) = imap_tls_server(&[&tokio_rustls::rustls::version::TLS12]).await;
        let info = connect(cert, port).await.connection_info();
        assert_eq!(info.tls_version, Some(TlsVersion::Tls1_2));
    }
}
