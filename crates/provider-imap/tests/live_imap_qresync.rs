//! Gated live integration: the IMAP CONDSTORE/QRESYNC incremental delta against the
//! Stalwart harness.
//!
//! Connects over implicit TLS (QRESYNC is negotiated on connect), snapshot-syncs the
//! dedicated `QResync` seed mailbox, then **mutates it on the server** — re-flags one
//! message and permanently expunges another via `edit_mail` — and runs a second sync.
//! Because the session is QRESYNC, that second sync is an incremental delta
//! (`CHANGEDSINCE`/`VANISHED`): it must reflect *both* the flag change and the expunge
//! in the store **without** a full re-snapshot. Detecting the expunge incrementally is
//! exactly what a non-QRESYNC delta cannot do, so a tombstoned message proves the path.
//!
//! Operates only on `QResync` (seeded by `docker/stalwart/seed.sh` with copies of the
//! first three INBOX fixtures), so it never disturbs the count-asserted
//! INBOX/Archive/Projects. Skips with no `STALWART_IMAP_ADDR`. Per the determinism
//! rule, targets are chosen by harness-controlled **subject**, never by server UID.

use core::time::Duration;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use engine_core::ids::{AccountId, MailboxId, ProviderKey};
use engine_core::mail::{Message, SystemKeyword};
use engine_core::sync::SyncScope;
use engine_provider::{MailEdit, Provider};
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::sync_mail;
use provider_imap::{ImapConfig, ImapProvider};
use serde::de::DeserializeOwned;
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

type Store = SqliteStore<ManualClock>;

/// A TLS connector that accepts the harness's self-signed cert. Test-only; it never
/// touches a host trust store. Mirrors the verifier in `live_imap.rs`.
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

/// Connects an `ImapProvider` bound to `mailbox` (QRESYNC negotiated on connect).
async fn connect(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        harness.account.as_str(),
        harness.password.as_str(),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

async fn load<T: DeserializeOwned>(store: &Store, scope: &SyncScope, key: &ProviderKey) -> T {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("deserialize stored object")
}

async fn messages_in(store: &Store, scope: &SyncScope) -> Vec<Message> {
    let mut out = Vec::new();
    for key in store.object_keys(scope).await.unwrap() {
        out.push(load::<Message>(store, scope, &key).await);
    }
    out
}

fn by_subject<'a>(messages: &'a [Message], subject: &str) -> &'a Message {
    messages
        .iter()
        .find(|m| m.envelope.subject.as_deref() == Some(subject))
        .unwrap_or_else(|| panic!("no seeded message with subject {subject:?}"))
}

#[tokio::test]
async fn live_qresync_delta_reconciles_flag_changes_and_expunges() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_qresync_delta_...: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-live-qresync").unwrap();
    let provider = connect(&harness, "QResync").await;
    let worker = || WorkerId::new("imap-live-qresync");

    // ---- Snapshot sync: the three seeded copies land and the cursor records the
    //      HIGHESTMODSEQ baseline (the QRESYNC SELECT carries it). ----
    sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("snapshot sync");

    let scope = provider.email_scope(&account);
    let before = messages_in(&store, &scope).await;
    assert_eq!(before.len(), 3, "QResync was seeded with three messages");

    // Targets by harness-controlled subject (never by server UID).
    let to_flag = by_subject(&before, "Harness baseline message");
    let to_delete = by_subject(&before, "Duplicate Message-ID (copy A)");
    assert!(
        !to_flag.has_system_keyword(SystemKeyword::Flagged),
        "the baseline starts unflagged"
    );
    let flagged_key = to_flag.id.key().clone();
    let deleted_key = to_delete.id.key().clone();

    // ---- Mutate on the server: flag one message, permanently expunge another. ----
    provider
        .edit_mail(&account, &MailEdit::set_flagged(flagged_key.clone(), true))
        .await
        .expect("flag a message");
    provider
        .edit_mail(&account, &MailEdit::delete(deleted_key.clone()))
        .await
        .expect("expunge a message");

    // ---- QRESYNC delta sync: reconciles BOTH changes incrementally. ----
    sync_mail(
        &provider,
        &store,
        &account,
        worker(),
        Duration::from_mins(5),
    )
    .await
    .expect("qresync delta sync");

    let after = messages_in(&store, &scope).await;

    // The expunged copy A is gone — a delta tombstone, which only QRESYNC's VANISHED
    // can deliver without a full re-snapshot.
    assert_eq!(
        after.len(),
        2,
        "the expunged message was tombstoned by the delta"
    );
    assert!(
        !after.iter().any(|m| m.id.key() == &deleted_key),
        "the expunged message must not linger in the store"
    );
    // The other duplicate (copy B) is untouched.
    assert!(
        after
            .iter()
            .any(|m| m.envelope.subject.as_deref() == Some("Duplicate Message-ID (copy B)")),
        "copy B is unaffected"
    );
    // The baseline now carries \Flagged — the delta applied the flag change.
    let reflagged = after
        .iter()
        .find(|m| m.id.key() == &flagged_key)
        .expect("the flagged message is still present");
    assert!(
        reflagged.has_system_keyword(SystemKeyword::Flagged),
        "the delta applied the server-side flag change"
    );
}

/// A test-only certificate verifier that accepts any server certificate, for the
/// harness's self-signed cert. Compiled only into this gated test; never reaches the
/// host store.
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
