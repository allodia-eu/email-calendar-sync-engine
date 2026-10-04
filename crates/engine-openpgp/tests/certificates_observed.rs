//! Certificates made by other implementations: RFC 9580's own vectors, and GnuPG
//! 2.4 (`tests/fixtures/gnupg/make.sh`).

mod support;

use engine_core::e2e::{
    AlgorithmName, CertificateProblem, CertificateStatus, Mechanism, Revocation, RevocationReach,
    SenderBinding, Warning,
};
use engine_openpgp::{Certificate, CertificateError, EncryptionPurpose};
use support::{RFC9580_A1, RFC9580_A3, at, gnupg, hex};

#[test]
fn the_rfc_9580_version_6_certificate_is_usable_and_binds_no_address() {
    // RFC 9580 A.3: a Direct Key signature and an X25519 subkey, no User ID.
    let cert = Certificate::parse(RFC9580_A3.as_bytes()).expect("A.3 parses");
    assert_eq!(cert.id().mechanism(), Mechanism::OpenPgp);
    assert_eq!(
        cert.id().to_string(),
        "CB186C4F0609A697E4D52DFA6C722B0C1F1E27C18A56708F6525EC27BAD9ACC9"
    );
    let view = cert.at(at("2024-01-01"));
    let facts = view.facts();
    assert_eq!(facts.status, CertificateStatus::Usable);
    assert_eq!(facts.created.to_string(), "2022-11-30T16:08:03Z");
    assert_eq!(facts.expires, None);
    assert!(facts.addresses.is_empty());
    assert!(facts.can_sign && facts.can_encrypt);
    // RFC 9580 §5.2.3.10: a key is usable without a User ID; binding needs one.
    assert_eq!(view.binds("anyone@example.org"), SenderBinding::NoAddress);
    let encryption = view.encryption_keys(EncryptionPurpose::ToOthers);
    assert_eq!(encryption.len(), 1);
    assert_eq!(
        hex(encryption[0].fingerprint()),
        "12C83F1E706F6308FE151A417743A1F033790E93E9978488D1DB378DA9930885"
    );
}

#[test]
fn a_bare_key_packet_has_no_valid_self_signature() {
    // RFC 9580 A.1: the key alone, nothing saying what it may do.
    let cert = Certificate::parse(RFC9580_A1.as_bytes()).expect("A.1 parses");
    assert_eq!(
        hex(cert.id().fingerprint()),
        "C959BDBAFA32A2F89A153B678CFDE12197965A9A"
    );
    assert_eq!(
        cert.at(at("2024-01-01")).status(),
        CertificateStatus::Unusable(CertificateProblem::NoValidSelfSignature)
    );
}

#[test]
fn a_gnupg_key_is_usable_for_its_address() {
    let cert = Certificate::parse(&gnupg("alice")).expect("parses");
    let view = cert.at(at("2025-03-01"));
    let facts = view.facts();
    assert_eq!(facts.status, CertificateStatus::Usable);
    assert_eq!(facts.addresses, ["alice@example.org"]);
    assert!(facts.can_sign && facts.can_encrypt);
    assert!(facts.warnings.is_empty());
    // Autocrypt canonicalisation: case does not decide the binding.
    assert_eq!(view.binds("Alice@EXAMPLE.org"), SenderBinding::Matches);
    assert_eq!(view.binds("bob@example.org"), SenderBinding::Mismatch);
    // The primary signs; the Curve25519 subkey encrypts.
    assert_eq!(view.signing_keys().len(), 1);
    assert!(view.signing_keys()[0].is_primary());
    let encryption = view.encryption_keys(EncryptionPurpose::ToOthers);
    assert_eq!(encryption.len(), 1);
    assert!(!encryption[0].is_primary());
}

#[test]
fn before_its_creation_a_key_is_not_yet_valid() {
    let cert = Certificate::parse(&gnupg("alice")).expect("parses");
    let view = cert.at(at("2024-12-31"));
    assert_eq!(view.status(), CertificateStatus::NotYetValid);
    assert!(view.signing_keys().is_empty());
    assert!(view.encryption_keys(EncryptionPurpose::ToOthers).is_empty());
}

#[test]
fn a_version_4_expiry_is_read_from_the_user_id_self_signature() {
    // GnuPG states expiry on the User ID self-signature, the v4 convention.
    let cert = Certificate::parse(&gnupg("expiring")).expect("parses");
    let before = cert.at(at("2025-12-31")).facts();
    assert_eq!(before.status, CertificateStatus::Usable);
    assert_eq!(
        before.expires.map(|t| t.to_string()).as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
    let after = cert.at(at("2026-01-01"));
    assert_eq!(after.status(), CertificateStatus::Expired);
    assert!(after.signing_keys().is_empty());
}

#[test]
fn an_rsa_2048_key_is_usable_with_a_warning() {
    // openpgp.md: RSA 2048 to 3071 works, with a warning (a recorded deviation).
    let facts = Certificate::parse(&gnupg("rsa2048"))
        .expect("parses")
        .at(at("2025-03-01"))
        .facts();
    assert_eq!(facts.status, CertificateStatus::Usable);
    assert!(facts.warnings.contains(&Warning::WeakPublicKey {
        algorithm: AlgorithmName::new("RSA"),
        bits: 2048,
    }));
}

#[test]
fn rsa_under_2048_bits_and_dsa_are_refused() {
    // RFC 9580 §12.4: MUST NOT sign or verify with RSA under 2048 bits; §12.5: MUST
    // NOT sign or verify using DSA keys.
    for name in ["rsa1024", "dsa"] {
        let view = Certificate::parse(&gnupg(name))
            .expect("parses")
            .at(at("2025-03-01"));
        assert_eq!(
            view.status(),
            CertificateStatus::Unusable(CertificateProblem::RejectedAlgorithm),
            "{name}"
        );
        assert!(view.signing_keys().is_empty(), "{name}");
        assert!(
            view.encryption_keys(EncryptionPurpose::ToOthers).is_empty(),
            "{name}"
        );
    }
}

#[test]
fn a_compromised_key_is_revoked_retroactively() {
    // RFC 9580 §5.2.3.31: "all signatures created by that key are suspect".
    let cert = Certificate::parse(&gnupg("revoked-compromised")).expect("parses");
    let revoked = CertificateStatus::Revoked(Revocation {
        at: at("2025-06-01"),
        reach: RevocationReach::Retroactive,
    });
    // Even at an instant before the revocation was made.
    assert_eq!(cert.at(at("2025-03-01")).status(), revoked);
    assert_eq!(cert.at(at("2025-07-01")).status(), revoked);
    assert!(cert.at(at("2025-03-01")).signing_keys().is_empty());
}

#[test]
fn a_superseded_key_is_revoked_from_then_on() {
    // RFC 9580 §5.2.3.31: "if it was merely superseded or retired, old signatures
    // are still valid".
    let cert = Certificate::parse(&gnupg("revoked-superseded")).expect("parses");
    assert_eq!(
        cert.at(at("2025-03-01")).status(),
        CertificateStatus::Usable
    );
    assert_eq!(cert.at(at("2025-03-01")).signing_keys().len(), 1);
    assert_eq!(
        cert.at(at("2025-07-01")).status(),
        CertificateStatus::Revoked(Revocation {
            at: at("2025-06-01"),
            reach: RevocationReach::FromThen,
        })
    );
}

#[test]
fn a_sha1_self_signature_counts_only_before_the_cut_off() {
    // openpgp.md: SHA-1 self-signatures made before 2023-02-01 count, with a warning.
    let old = Certificate::parse(&gnupg("sha1-self-signature-2020"))
        .expect("parses")
        .at(at("2025-03-01"))
        .facts();
    assert_eq!(old.status, CertificateStatus::Usable);
    assert!(old.warnings.contains(&Warning::DeprecatedHash {
        hash: AlgorithmName::new("SHA1"),
    }));
    // RFC 9580 §9.5: a recent one never validates.
    let new = Certificate::parse(&gnupg("sha1-self-signature-2024"))
        .expect("parses")
        .at(at("2025-03-01"));
    assert_eq!(
        new.status(),
        CertificateStatus::Unusable(CertificateProblem::NoValidSelfSignature)
    );
}

#[test]
fn armoured_and_binary_forms_parse_the_same() {
    let armoured = Certificate::parse(RFC9580_A3.as_bytes()).expect("armoured");
    let binary = Certificate::parse(&armoured.to_bytes()).expect("binary");
    assert_eq!(binary.id(), armoured.id());
    assert_eq!(
        binary.at(at("2024-01-01")).facts(),
        armoured.at(at("2024-01-01")).facts()
    );
}

#[test]
fn input_that_is_not_exactly_one_certificate_is_refused() {
    assert_eq!(
        Certificate::parse(b"").err(),
        Some(CertificateError::Malformed)
    );
    assert_eq!(
        Certificate::parse(b"not a key at all").err(),
        Some(CertificateError::Malformed)
    );
    let mut two = gnupg("alice");
    two.extend(gnupg("expiring"));
    assert_eq!(
        Certificate::parse(&two).err(),
        Some(CertificateError::NotOne)
    );
}
