//! Certificate exceptions: one server's certificate, trusted because a person decided
//! to trust it.
//!
//! Normal verification runs first and is never relaxed. An exception is consulted only
//! once it has failed, and it matches one server name and one exact certificate, so it
//! is worth nothing to anything that does not present that certificate.
//!
//! This is the only mechanism that reaches a server whose certificate cannot be made
//! verifiable by adding a root. A self-signed CA served as its own end-entity
//! certificate — what Proton Mail Bridge and several self-hosted servers do — fails
//! with `CaUsedAsEndEntity`, which the webpki verifier decides from the leaf's own
//! basic constraints *before* it consults a trust anchor, so adding that certificate to
//! [`TlsPolicy::Roots`](crate::TlsPolicy)'s `custom` set changes nothing.
//!
//! The same shape covers an expired certificate and a name mismatch: an exception
//! overrides whatever the verifier objected to, for that one certificate.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

/// The SHA-256 fingerprint of a DER-encoded certificate: what an exception matches on,
/// and the identity a host shows a person who is being asked to accept one.
#[must_use]
pub fn fingerprint(certificate: &CertificateDer<'_>) -> [u8; 32] {
    let digest = Sha256::digest(certificate.as_ref());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// A certificate a person has explicitly accepted for one server, though it fails
/// normal verification.
///
/// Scoped to the TLS **server name** and to one exact certificate: that server
/// presenting anything else is refused as it would have been, and no other server is
/// affected at all. A host stores these with the account they were accepted for, so an
/// exception ends when the account does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateException {
    server_name: String,
    fingerprint: [u8; 32],
}

impl CertificateException {
    /// Accepts `certificate` when `server_name` presents it.
    #[must_use]
    pub fn new(server_name: &str, certificate: &CertificateDer<'_>) -> Self {
        Self::from_fingerprint(server_name, fingerprint(certificate))
    }

    /// Accepts the certificate with this SHA-256 `fingerprint` when `server_name`
    /// presents it — the form a host rebuilds from its own storage.
    ///
    /// `server_name` must be the name the **handshake** asks for, which for an address
    /// is [`std::net::IpAddr`]'s own rendering: compressed and unbracketed
    /// (`2001:db8::1`, never `[2001:db8::1]` or an expanded form). An exception whose
    /// name is spelled otherwise simply never matches, so it fails closed and the
    /// person is asked again. [`RejectedCertificate::exception`] always produces the
    /// right spelling, and is the way to avoid the question.
    #[must_use]
    pub fn from_fingerprint(server_name: &str, fingerprint: [u8; 32]) -> Self {
        Self {
            server_name: normalized(server_name),
            fingerprint,
        }
    }

    /// The TLS server name this exception is scoped to.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The SHA-256 fingerprint of the accepted certificate.
    #[must_use]
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// Whether this exception covers `certificate` presented by `server_name` (already
    /// normalized by the caller).
    fn accepts(&self, server_name: &str, certificate: &CertificateDer<'_>) -> bool {
        self.server_name == server_name && self.fingerprint == fingerprint(certificate)
    }
}

/// What a server this config refused presented, so a host can show a person exactly
/// what it declined to trust rather than only that something was wrong.
#[derive(Clone)]
pub struct RejectedCertificate {
    server_name: String,
    certificate: CertificateDer<'static>,
}

impl core::fmt::Debug for RejectedCertificate {
    /// Terse, and deliberately so: the derived form prints the whole certificate as a
    /// byte vector beside the server somebody reads their mail from, and this type
    /// travels inside a connect error a host is likely to log. The fingerprint names it
    /// without carrying it.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RejectedCertificate")
            .field("sha256", &hex(&self.fingerprint()))
            .finish_non_exhaustive()
    }
}

/// A fingerprint as lowercase hex, for [`RejectedCertificate`]'s `Debug`.
fn hex(fingerprint: &[u8; 32]) -> String {
    use core::fmt::Write as _;
    fingerprint.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

impl RejectedCertificate {
    /// The TLS server name the client asked for.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The end-entity certificate that server presented.
    #[must_use]
    pub fn certificate(&self) -> &CertificateDer<'static> {
        &self.certificate
    }

    /// Its SHA-256 fingerprint: what an exception for it carries.
    #[must_use]
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&self.certificate)
    }

    /// The exception that would accept it.
    #[must_use]
    pub fn exception(&self) -> CertificateException {
        CertificateException::from_fingerprint(&self.server_name, self.fingerprint())
    }

    /// What the certificate claims about itself, for a host to show beside the
    /// fingerprint — `None` when the bytes do not parse, which an unvalidated
    /// certificate is entitled to be.
    #[must_use]
    pub fn summary(&self) -> Option<crate::CertificateSummary> {
        crate::CertificateSummary::read(&self.certificate)
    }
}

/// Where a config records the last certificate it refused. Shared with the verifier by
/// `Arc`, so a clone of the config reads the same slot.
#[derive(Clone, Debug, Default)]
pub(crate) struct RejectionSlot(Arc<Mutex<Option<RejectedCertificate>>>);

impl RejectionSlot {
    /// The last refusal, if any.
    pub(crate) fn last(&self) -> Option<RejectedCertificate> {
        self.lock().clone()
    }

    fn record(&self, rejected: RejectedCertificate) {
        *self.lock() = Some(rejected);
    }

    fn clear(&self) {
        *self.lock() = None;
    }

    /// A poisoned slot still holds a perfectly good certificate: the panic that poisoned
    /// it happened elsewhere, and refusing to read a diagnostic because of it would turn
    /// one failure into two.
    fn lock(&self) -> MutexGuard<'_, Option<RejectedCertificate>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Wraps the policy's own verifier: accepts a server whose certificate a person has
/// excepted, and records the certificate of every server it refuses.
///
/// The inner verifier runs first and unchanged, so an exception can only ever *follow*
/// a failure. Nothing here can weaken a server that verifies normally.
#[derive(Debug)]
pub(crate) struct ExceptionVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    exceptions: Vec<CertificateException>,
    rejected: RejectionSlot,
}

impl ExceptionVerifier {
    pub(crate) fn new(
        inner: Arc<dyn ServerCertVerifier>,
        exceptions: &[CertificateException],
        rejected: RejectionSlot,
    ) -> Self {
        Self {
            inner,
            exceptions: exceptions.to_vec(),
            rejected,
        }
    }
}

impl ServerCertVerifier for ExceptionVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let Err(error) = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) else {
            // Nothing to answer any more: a config that has since reached this server
            // must not still be holding it out as a question.
            self.rejected.clear();
            return Ok(ServerCertVerified::assertion());
        };
        let name = normalized(&server_name.to_str());
        if self
            .exceptions
            .iter()
            .any(|exception| exception.accepts(&name, end_entity))
        {
            self.rejected.clear();
            return Ok(ServerCertVerified::assertion());
        }
        self.rejected.record(RejectedCertificate {
            server_name: name,
            certificate: end_entity.clone().into_owned(),
        });
        Err(error)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    /// Forwarded rather than left to the trait's default. This wrapper is on the path of
    /// **every** config, the plain [`client_config`](crate::client_config) included, so
    /// a default here would quietly answer for an inner verifier that had said otherwise.
    /// Neither `WebPkiServerVerifier` nor `rustls-platform-verifier` overrides these
    /// today; that is a fact about their current versions, not a property to build on.
    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}

/// One spelling for a server name, so a stored exception matches what the handshake
/// asked for. rustls already lowercases a DNS name; an IP address arrives as its
/// textual form and a host's stored copy may not have been through either.
fn normalized(server_name: &str) -> String {
    server_name.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use rustls::pki_types::CertificateDer;

    use super::{CertificateException, RejectionSlot, fingerprint, normalized};

    fn der(bytes: &[u8]) -> CertificateDer<'static> {
        CertificateDer::from(bytes.to_vec())
    }

    #[test]
    fn an_exception_matches_only_its_own_server_and_certificate() {
        let cert = der(b"the certificate");
        let other = der(b"another certificate");
        let exception = CertificateException::new("mail.example.com", &cert);

        assert!(exception.accepts("mail.example.com", &cert));
        assert!(!exception.accepts("mail.example.com", &other));
        assert!(!exception.accepts("imap.example.net", &cert));
    }

    #[test]
    fn a_server_name_matches_whatever_case_it_was_stored_in() {
        let cert = der(b"the certificate");
        let exception = CertificateException::new("  MAIL.Example.COM ", &cert);

        assert_eq!(exception.server_name(), "mail.example.com");
        assert!(exception.accepts(&normalized("mail.example.com"), &cert));
    }

    #[test]
    fn a_stored_fingerprint_rebuilds_the_same_exception() {
        let cert = der(b"the certificate");
        let minted = CertificateException::new("mail.example.com", &cert);
        let restored =
            CertificateException::from_fingerprint(minted.server_name(), minted.fingerprint());

        assert_eq!(minted, restored);
        assert!(restored.accepts("mail.example.com", &cert));
    }

    #[test]
    fn the_fingerprint_is_the_sha256_of_the_der() {
        // The published SHA-256 of the empty input, so this pins the algorithm rather
        // than agreeing with itself.
        let empty = fingerprint(&der(b""));
        assert_eq!(
            hex(&empty),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_slot_holds_the_last_refusal_and_offers_its_exception() {
        let slot = RejectionSlot::default();
        assert!(slot.last().is_none());

        slot.record(super::RejectedCertificate {
            server_name: "mail.example.com".to_owned(),
            certificate: der(b"the certificate"),
        });
        let rejected = slot.last().expect("a refusal was recorded");
        assert_eq!(rejected.server_name(), "mail.example.com");
        assert_eq!(
            rejected.fingerprint(),
            fingerprint(&der(b"the certificate"))
        );
        assert!(
            rejected
                .exception()
                .accepts("mail.example.com", &der(b"the certificate"))
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }
}
