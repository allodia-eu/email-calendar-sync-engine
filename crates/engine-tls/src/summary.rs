//! What a certificate says about itself.
//!
//! Somebody asked to accept a certificate that did not verify has one question: is this
//! the server I meant? The names in the certificate and the dates it is valid between
//! are what answers it, so a host reads them out of the DER and shows them beside the
//! fingerprint.
//!
//! These are the server's own claims, and nothing has vouched for them — that is what
//! failed. Nothing here verifies anything, and no decision may rest on it beyond a
//! person's own.

use rustls::pki_types::CertificateDer;
use x509_cert::{Certificate, der::Decode, name::Name};

/// The identity a certificate claims, and the window it claims to be valid in.
///
/// Times are seconds since the Unix epoch. This crate formats no dates: it carries no
/// timezone data and the host knows the reader's locale, so the host formats them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateSummary {
    subject_common_name: Option<String>,
    subject_organization: Option<String>,
    issuer_common_name: Option<String>,
    issuer_organization: Option<String>,
    not_before: i64,
    not_after: i64,
}

impl CertificateSummary {
    /// Reads `certificate`, or `None` when it is not a certificate this can parse.
    ///
    /// A server sent these bytes and nothing has validated them, so failing to read
    /// them is an ordinary outcome: the caller still has the fingerprint, which is
    /// taken over the bytes themselves and needs no parse.
    #[must_use]
    pub fn read(certificate: &CertificateDer<'_>) -> Option<Self> {
        let parsed = Certificate::from_der(certificate.as_ref()).ok()?;
        let tbs = parsed.tbs_certificate();
        Some(Self {
            subject_common_name: common_name(tbs.subject()),
            subject_organization: organization(tbs.subject()),
            issuer_common_name: common_name(tbs.issuer()),
            issuer_organization: organization(tbs.issuer()),
            not_before: epoch_seconds(tbs.validity().not_before),
            not_after: epoch_seconds(tbs.validity().not_after),
        })
    }

    /// The subject's common name (`CN`) — for a server certificate, the name it claims
    /// to be.
    #[must_use]
    pub fn subject_common_name(&self) -> Option<&str> {
        self.subject_common_name.as_deref()
    }

    /// The subject's organization (`O`).
    #[must_use]
    pub fn subject_organization(&self) -> Option<&str> {
        self.subject_organization.as_deref()
    }

    /// The issuer's common name (`CN`). Equal to the subject's on a self-signed
    /// certificate, which is how a host can say so.
    #[must_use]
    pub fn issuer_common_name(&self) -> Option<&str> {
        self.issuer_common_name.as_deref()
    }

    /// The issuer's organization (`O`).
    #[must_use]
    pub fn issuer_organization(&self) -> Option<&str> {
        self.issuer_organization.as_deref()
    }

    /// When the certificate claims to become valid, in seconds since the Unix epoch.
    #[must_use]
    pub fn not_before(&self) -> i64 {
        self.not_before
    }

    /// When the certificate claims to expire, in seconds since the Unix epoch.
    #[must_use]
    pub fn not_after(&self) -> i64 {
        self.not_after
    }
}

fn common_name(name: &Name) -> Option<String> {
    Some(name.common_name().ok()??.value().into_owned())
}

fn organization(name: &Name) -> Option<String> {
    Some(name.organization().ok()??.value().into_owned())
}

/// X.509 times are absolute and carry no zone. `to_unix_duration` saturates at the
/// epoch for the few certificates dated before it, which is as close as a display can
/// get anyway.
fn epoch_seconds(time: x509_cert::time::Time) -> i64 {
    i64::try_from(time.to_unix_duration().as_secs()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::CertificateSummary;
    use rustls::pki_types::CertificateDer;

    /// A self-signed certificate naming `common` and `organization`.
    fn certificate(common: &str, organization: &str) -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().expect("key pair");
        let mut params =
            rcgen::CertificateParams::new(vec!["mail.example.com".to_owned()]).expect("params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common);
        params
            .distinguished_name
            .push(rcgen::DnType::OrganizationName, organization);
        params.self_signed(&key).expect("self-signed").der().clone()
    }

    #[test]
    fn a_certificate_reports_the_names_it_claims() {
        let summary = CertificateSummary::read(&certificate("mail.example.com", "Example Ltd"))
            .expect("a well-formed certificate parses");

        assert_eq!(summary.subject_common_name(), Some("mail.example.com"));
        assert_eq!(summary.subject_organization(), Some("Example Ltd"));
        // Self-signed, so the issuer is the subject — the fact a host renders as
        // "issued by itself".
        assert_eq!(summary.issuer_common_name(), Some("mail.example.com"));
        assert_eq!(summary.issuer_organization(), Some("Example Ltd"));
    }

    #[test]
    fn a_certificate_reports_the_window_it_claims() {
        let summary = CertificateSummary::read(&certificate("mail.example.com", "Example Ltd"))
            .expect("read");

        assert!(
            summary.not_before() < summary.not_after(),
            "a validity window runs forwards: {} to {}",
            summary.not_before(),
            summary.not_after()
        );
        // rcgen's default window brackets now, which is what makes the dates readable.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs(),
        )
        .expect("within i64");
        assert!(summary.not_before() <= now && now <= summary.not_after());
    }

    #[test]
    fn bytes_that_are_not_a_certificate_read_as_nothing() {
        assert!(CertificateSummary::read(&CertificateDer::from(vec![0u8, 1, 2, 3])).is_none());
    }
}
