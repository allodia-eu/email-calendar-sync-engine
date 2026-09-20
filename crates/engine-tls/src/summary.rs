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

use std::net::{Ipv4Addr, Ipv6Addr};

use rustls::pki_types::CertificateDer;
use x509_cert::{
    Certificate,
    der::Decode,
    ext::pkix::{SubjectAltName, name::GeneralName},
    name::Name,
};

/// The longest a claimed name may be before it is cut. A host renders these in a dialog,
/// and nothing has vouched for their length any more than for their content.
const MAX_CLAIM: usize = 128;

/// The identity a certificate claims, and the window it claims to be valid in.
///
/// Times are seconds since the Unix epoch. This crate formats no dates: it carries no
/// timezone data and the host knows the reader's locale, so the host formats them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateSummary {
    subject_names: Vec<String>,
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
            subject_names: subject_names(&parsed),
            subject_common_name: common_name(tbs.subject()),
            subject_organization: organization(tbs.subject()),
            issuer_common_name: common_name(tbs.issuer()),
            issuer_organization: organization(tbs.issuer()),
            not_before: epoch_seconds(tbs.validity().not_before),
            not_after: epoch_seconds(tbs.validity().not_after),
        })
    }

    /// The names this certificate is **valid for**: its `subjectAltName` DNS names and
    /// IP addresses, in the order the certificate lists them.
    ///
    /// This is the one field that answers "is this the server I meant?", because it is
    /// the one a verifier reads. Prefer it over [`Self::subject_common_name`] wherever a
    /// host shows a name to somebody deciding whether to accept a certificate. Empty
    /// when the certificate carries no `subjectAltName`, which is itself a reason such a
    /// certificate cannot verify.
    #[must_use]
    pub fn subject_names(&self) -> &[String] {
        &self.subject_names
    }

    /// The subject's common name (`CN`).
    ///
    /// **A label, not a name claim.** Nothing checks a certificate's `CN` against the
    /// host that was dialled: `rustls-webpki` reads `subjectAltName` and has no `CN`
    /// fallback at all. A certificate whose `CN` is the server somebody expected and
    /// whose `subjectAltName` is something else fails to verify *because of the latter*,
    /// so a host showing this as the name would be showing the one field the refusal did
    /// not turn on. Use [`Self::subject_names`].
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
    /// certificate, which is how a host can say so. A label, as
    /// [`Self::subject_common_name`] is, but an issuer has no `subjectAltName` to prefer.
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

/// The `subjectAltName` DNS names and IP addresses, which are what a verifier matches a
/// host against. Other general-name kinds (an email address, a URI, a directory name) are
/// left out: none of them names a server, and each would only add something to misread.
fn subject_names(certificate: &Certificate) -> Vec<String> {
    let Ok(Some((_critical, san))) = certificate
        .tbs_certificate()
        .get_extension::<SubjectAltName>()
    else {
        return Vec::new();
    };
    san.0
        .iter()
        .filter_map(|name| match name {
            GeneralName::DnsName(dns) => Some(claim(dns.as_str())),
            GeneralName::IpAddress(address) => ip_address(address.as_bytes()).map(|ip| claim(&ip)),
            _ => None,
        })
        .collect()
}

/// An `iPAddress` general name, which RFC 5280 carries as raw octets: four for IPv4,
/// sixteen for IPv6. Anything else is not an address and is dropped.
fn ip_address(octets: &[u8]) -> Option<String> {
    match octets.len() {
        4 => Some(Ipv4Addr::from(<[u8; 4]>::try_from(octets).ok()?).to_string()),
        16 => Some(Ipv6Addr::from(<[u8; 16]>::try_from(octets).ok()?).to_string()),
        _ => None,
    }
}

fn common_name(name: &Name) -> Option<String> {
    Some(claim(&name.common_name().ok()??.value()))
}

fn organization(name: &Name) -> Option<String> {
    Some(claim(&name.organization().ok()??.value()))
}

/// One claimed string, made safe to put in front of somebody.
///
/// The value comes verbatim from a certificate nothing has vouched for, and its whole
/// purpose is to be rendered in a dialog asking somebody to trust that certificate. A
/// `DirectoryString` may be UTF-8, so a claim can carry newlines, bidirectional overrides
/// and any length it likes: a `CN` reading `mail.example.com\n\nVerified by a trusted
/// authority` would otherwise arrive at a host as a name to print. Control and formatting
/// characters become separators, runs of whitespace collapse, and the result is cut to
/// [`MAX_CLAIM`].
fn claim(value: &str) -> String {
    let mut out = String::new();
    let mut length = 0usize;
    let mut spaced = false;
    for character in value.chars() {
        // `is_control` misses the bidi and annotation formatting characters, which are
        // exactly the ones used to make a string read as something it is not.
        if character.is_control() || character.is_whitespace() || is_format(character) {
            spaced = length > 0;
            continue;
        }
        if spaced {
            if length >= MAX_CLAIM {
                out.push('…');
                return out;
            }
            out.push(' ');
            length += 1;
            spaced = false;
        }
        if length >= MAX_CLAIM {
            out.push('…');
            return out;
        }
        out.push(character);
        length += 1;
    }
    out
}

/// Whether `character` is a Unicode format control: the bidirectional overrides and
/// embeddings, the zero-width joiners, and the interlinear annotations.
fn is_format(character: char) -> bool {
    matches!(character,
        '\u{00ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}')
}

/// X.509 times are absolute and carry no zone. `to_unix_duration` saturates at the epoch
/// for the few certificates dated before it, which is as close as a display can get anyway.
fn epoch_seconds(time: x509_cert::time::Time) -> i64 {
    i64::try_from(time.to_unix_duration().as_secs()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use rustls::pki_types::CertificateDer;

    use super::{CertificateSummary, claim};

    /// A self-signed certificate naming `common` and `organization`, valid for `san`.
    fn certificate(common: &str, organization: &str, san: &[&str]) -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().expect("key pair");
        let names = san
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        let mut params = rcgen::CertificateParams::new(names).expect("params");
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
        let summary = CertificateSummary::read(&certificate(
            "mail.example.com",
            "Example Ltd",
            &["mail.example.com"],
        ))
        .expect("a well-formed certificate parses");

        assert_eq!(summary.subject_common_name(), Some("mail.example.com"));
        assert_eq!(summary.subject_organization(), Some("Example Ltd"));
        // Self-signed, so the issuer is the subject — the fact a host renders as
        // "issued by itself".
        assert_eq!(summary.issuer_common_name(), Some("mail.example.com"));
        assert_eq!(summary.issuer_organization(), Some("Example Ltd"));
    }

    /// The names a verifier reads are the `subjectAltName` entries, not the `CN`, so a
    /// certificate whose `CN` says one thing and whose SAN says another reports the SAN.
    /// This is the shape that makes a `CN`-based display dangerous: it would show the
    /// server somebody expected while the certificate is valid for another.
    #[test]
    fn the_names_it_is_valid_for_come_from_the_alt_names() {
        let summary = CertificateSummary::read(&certificate(
            "mail.example.com",
            "Example Ltd",
            &["elsewhere.example.net", "other.example.org"],
        ))
        .expect("read");

        assert_eq!(
            summary.subject_names(),
            [
                "elsewhere.example.net".to_owned(),
                "other.example.org".to_owned()
            ]
        );
        assert_eq!(summary.subject_common_name(), Some("mail.example.com"));
    }

    /// An `iPAddress` alt name is octets on the wire; it reaches a host as an address.
    #[test]
    fn an_address_alt_name_reads_as_an_address() {
        let summary =
            CertificateSummary::read(&certificate("127.0.0.1", "Example Ltd", &["127.0.0.1"]))
                .expect("read");

        assert_eq!(summary.subject_names(), ["127.0.0.1".to_owned()]);
    }

    #[test]
    fn a_certificate_reports_the_window_it_claims() {
        let summary = CertificateSummary::read(&certificate(
            "mail.example.com",
            "Example Ltd",
            &["mail.example.com"],
        ))
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

    /// A claim is rendered in a dialog asking somebody to trust the certificate it came
    /// from, so it may not carry the characters that would make it read as something else.
    #[test]
    fn a_claim_cannot_forge_lines_of_its_own() {
        assert_eq!(
            claim("mail.example.com\n\nVerified by a trusted authority"),
            "mail.example.com Verified by a trusted authority"
        );
        // A right-to-left override reverses everything after it on screen. It becomes a
        // separator rather than nothing, so what it joined stays legibly two things.
        assert_eq!(claim("mail\u{202e}example.com"), "mail example.com");
        assert_eq!(claim("\tmail.example.com  "), "mail.example.com");
    }

    #[test]
    fn a_claim_is_cut_to_a_length_a_dialog_can_hold() {
        let cut = claim(&"a".repeat(500));
        assert_eq!(cut.chars().count(), 129, "128 characters and the ellipsis");
        assert!(cut.ends_with('…'));
    }
}
