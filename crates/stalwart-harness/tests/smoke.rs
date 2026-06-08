//! Connectivity smoke suite for the Stalwart harness.
//!
//! This is the harness's own correctness gate: it proves the seeded server is
//! up, answers on every protocol for the seeded account, and that the shared
//! dataset is present (including the copy/move mailbox memberships). The deep
//! protocol suites (JMAP `Email/changes`, IMAP `UIDVALIDITY`, CalDAV sync-token,
//! …) belong to the provider clients in build-order steps 4–5, not here.
//!
//! Gating contract: every test calls [`gate`], which returns `None` (and prints
//! a skip line) unless `STALWART_HTTP_ADDR` is set. With no Docker the whole
//! suite no-ops, so `cargo test --workspace` stays green offline.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use stalwart_harness::{GATE_VAR, Harness, ImapProbe, ONE_OFF_EVENT_UID, run_probe};

/// Return the configured harness, or `None` (skipping, with a note labelled by
/// the current test's name) when Stalwart is absent.
fn gate() -> Option<Harness> {
    let harness = Harness::from_env();
    if harness.is_none() {
        let test = std::thread::current().name().unwrap_or("test").to_owned();
        eprintln!("skipping `{test}`: set {GATE_VAR} (and run docker compose) to exercise it");
    }
    harness
}

/// Like [`gate`], but also block until the server is ready — the precondition
/// every protocol probe shares. `None` (skipping) when Stalwart is absent.
fn ready_harness() -> Option<Harness> {
    let harness = gate()?;
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("Stalwart should become ready");
    Some(harness)
}

#[test]
fn server_becomes_ready() {
    let Some(h) = gate() else {
        return;
    };
    h.wait_until_ready(Duration::from_secs(30))
        .expect("Stalwart /healthz/live should report ready");
}

#[test]
fn jmap_session_advertises_core() {
    let Some(h) = ready_harness() else {
        return;
    };
    let session = h.jmap_session().expect("JMAP session should be fetchable");
    let caps = session
        .get("capabilities")
        .and_then(|c| c.as_object())
        .expect("session has a capabilities object");
    assert!(
        caps.contains_key("urn:ietf:params:jmap:core"),
        "JMAP core capability must be advertised, got: {:?}",
        caps.keys().collect::<Vec<_>>()
    );
}

#[test]
fn imap_answers_and_seed_present() {
    let Some(h) = ready_harness() else {
        return;
    };
    let probe = imap_probe_over_tls(&h);
    assert!(
        probe.greeting.starts_with("* OK"),
        "IMAP greeting: {:?}",
        probe.greeting
    );
    // INBOX holds the 8 appended fixtures (the moved message left, the copied
    // one stays); allow extras but require at least the seeded baseline.
    assert!(
        probe.inbox_exists >= 8,
        "expected >= 8 seeded INBOX messages, saw {}",
        probe.inbox_exists
    );
    // The baseline message was COPYed into Archive: same content, two mailbox
    // memberships (distinct provider objects).
    assert_eq!(
        probe.archive_exists, 1,
        "Archive should hold the copied message"
    );
    // One message was MOVEd into Projects: a single membership there.
    assert_eq!(
        probe.projects_exists, 1,
        "Projects should hold the moved message"
    );
    // Two messages share one Message-ID and are stored distinctly; both carry
    // "Duplicate" in their subject.
    assert_eq!(
        probe.dup_subject_hits, 2,
        "the duplicate-Message-ID pair should both be present in INBOX"
    );
}

#[test]
fn smtp_banner_greets() {
    let Some(h) = ready_harness() else {
        return;
    };
    let banner = h.smtp_banner().expect("SMTP should greet");
    assert!(banner.starts_with("220"), "SMTP banner: {banner:?}");
}

#[test]
fn caldav_lists_default_calendar() {
    let Some(h) = ready_harness() else {
        return;
    };
    let resp = h
        .caldav_propfind(&h.calendar_collection_path())
        .expect("CalDAV PROPFIND should answer");
    assert_eq!(
        resp.status,
        207,
        "CalDAV PROPFIND should be 207 Multi-Status, got {} ({})",
        resp.status,
        resp.body_text()
    );
}

#[test]
fn caldav_one_off_event_present() {
    let Some(h) = ready_harness() else {
        return;
    };
    let resp = h
        .caldav_get(&h.event_path(ONE_OFF_EVENT_UID))
        .expect("CalDAV GET should answer");
    assert_eq!(resp.status, 200, "GET one-off event status");
    assert!(
        resp.body_contains("One-off zoned event"),
        "seeded one-off event SUMMARY should be present in: {}",
        resp.body_text()
    );
}

/// Connect to the implicit-TLS IMAP listener and run the probe. The server uses
/// a self-signed test certificate, so the verifier below accepts any cert —
/// this is a probe against a known local fixture, never the host trust store.
fn imap_probe_over_tls(h: &Harness) -> ImapProbe {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(no_verify::AcceptAny))
        .with_no_client_auth();
    let host = h
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host)
        .to_owned();
    let server_name =
        rustls::pki_types::ServerName::try_from(host).expect("IMAP host is a valid server name");
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .expect("rustls client connection");
    // No socket read timeout, for the same reason as the SMTP probe (see
    // smtp.rs): a `SO_RCVTIMEO` was observed not to wake through Docker
    // Desktop's macOS port proxy. The server is health-gated before this runs.
    let tcp = TcpStream::connect(&h.imap_addr).expect("connect IMAP TLS port");
    let tls = rustls::StreamOwned::new(conn, tcp);
    run_probe(tls, &h.account, &h.password).expect("IMAP probe should succeed")
}

/// A no-op certificate verifier for the harness's self-signed test cert. This is
/// deliberately insecure and lives only in the test binary; never reuse it.
mod no_verify {
    use rustls::DigitallySignedStruct;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    #[derive(Debug)]
    pub(crate) struct AcceptAny;

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}
