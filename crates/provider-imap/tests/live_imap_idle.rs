//! Gated live integration: IMAP IDLE (RFC 2177) push against the Stalwart harness.
//!
//! Opens an [`ImapWatcher`] on the dedicated `Idle` seed mailbox (its own standing
//! connection), then — on a **separate** connection — flag-toggles the seeded message
//! with `edit_mail`. A flag change on a mailbox an IDLE session is watching makes the
//! server push an unsolicited `* n FETCH (FLAGS …)`, which the watcher must surface as
//! [`WatchEvent::Changed`]. This proves the end-to-end push path (negotiate IDLE,
//! `EXAMINE`, `IDLE`/continuation, classify the notification) against a real server.
//!
//! Operates only on `Idle` (seeded by `docker/stalwart/seed.sh` with one INBOX copy),
//! so it never disturbs the count-asserted INBOX/Archive/Projects or the `QResync`
//! mailbox the QRESYNC delta test mutates. Skips with no `STALWART_IMAP_ADDR`. The
//! watcher and the mutator are deliberately distinct connections — push needs the
//! watch socket to keep idling while another session makes the change.

use std::sync::Arc;
use std::time::Duration;

use engine_core::ids::{AccountId, MailboxId};
use engine_provider::{MailEdit, Provider, WatchEvent};
use provider_imap::{ImapConfig, ImapProvider, ImapWatcher};
use stalwart_harness::Harness;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// A TLS connector that accepts the harness's self-signed cert. Test-only; it never
/// touches a host trust store. Mirrors the verifier in `live_imap_qresync.rs`.
fn no_verify_connector() -> TlsConnector {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(no_verify::AcceptAny))
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

fn config_for(harness: &Harness) -> ImapConfig {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        harness.account.as_str(),
        harness.password.as_str(),
    )
}

async fn connect(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    ImapProvider::connect(
        &config_for(harness),
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_idle_pushes_a_change_notification() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_idle_pushes_a_change_notification: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");

    let account = AccountId::try_from("imap-live-idle").unwrap();

    // A mutator connection to flag-toggle the watched mailbox, and the one seeded
    // message's key (the toggle target). The key is from this same connection, so its
    // UIDVALIDITY is current for the edit.
    let mutator = connect(&harness, "Idle").await;
    let page = mutator
        .sync_email_page(&account, None, None, 10)
        .await
        .expect("sync Idle");
    let key = page
        .changed
        .first()
        .expect("Idle was seeded with one message")
        .id
        .key()
        .clone();

    // The watcher on its own dedicated connection (a short keep-alive; the change is
    // expected within seconds, well before it would matter).
    let watcher = ImapWatcher::connect(
        &config_for(&harness),
        no_verify_connector(),
        MailboxId::try_from("Idle").unwrap(),
        Duration::from_secs(20),
    )
    .await
    .expect("watcher (Stalwart advertises IDLE)");

    // Drive the watch in a task; it blocks in IDLE until the server pushes a change.
    let watch = tokio::spawn(async move {
        let mut watcher = watcher;
        watcher.next_event().await
    });

    // Toggle the flag every 300 ms until the watcher reports the change. Toggling (not
    // re-setting) guarantees each `STORE` is a real change, so the server emits a fresh
    // unsolicited FETCH to the idling session even if an earlier one raced ahead of it.
    let mut flag = true;
    for _ in 0..40 {
        if watch.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = mutator
            .edit_mail(&account, &MailEdit::set_flagged(key.clone(), flag))
            .await;
        flag = !flag;
    }

    let event = tokio::time::timeout(Duration::from_secs(20), watch)
        .await
        .expect("watcher reported within 20s")
        .expect("watch task joined")
        .expect("watch event");
    assert_eq!(
        event,
        WatchEvent::Changed,
        "a server-side flag change pushes a Changed notification to the idling watcher"
    );
}

/// A test-only certificate verifier that accepts any server certificate, for the
/// harness's self-signed cert. Compiled only into this gated test; never reaches the
/// host store. Mirrors `live_imap_qresync.rs`.
mod no_verify {
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::crypto::ring::default_provider;
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug)]
    pub(super) struct AcceptAny;

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}
