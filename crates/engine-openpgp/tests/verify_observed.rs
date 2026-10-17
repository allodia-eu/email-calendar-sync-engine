//! Verifying signatures GnuPG 2.4 made (`tests/fixtures/gnupg/make.sh`), each over
//! the same MIME entity, on 2025-02-01.

mod support;

use engine_core::e2e::{
    AlgorithmName, IssuerHint, SenderBinding, SignatureVerdict, UnusableReason, Warning,
};
use engine_openpgp::{Certificate, Verifier};
use support::{at, gnupg, gnupg_signature, signed_entity};

fn certificate(name: &str) -> Certificate {
    Certificate::parse(&gnupg(name)).expect("a GnuPG certificate parses")
}

/// The single verdict on GnuPG's signature `name`, checked against `certificates`.
fn verdict(name: &str, certificates: &[Certificate], sender: &str, now: &str) -> SignatureVerdict {
    let verdicts = Verifier::new(certificates, sender, at(now))
        .detached(&signed_entity(), &gnupg_signature(name));
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    verdicts.into_iter().next().expect("one verdict")
}

#[test]
fn a_signature_by_a_usable_key_is_good_and_names_its_sender() {
    let alice = [certificate("alice")];
    let SignatureVerdict::Good(good) = verdict("alice", &alice, "Alice@example.org", "2025-03-01")
    else {
        panic!("expected a good signature");
    };
    assert_eq!(&good.signer, alice[0].id());
    assert_eq!(good.created, at("2025-02-01"));
    assert_eq!(good.sender, SenderBinding::Matches);
    assert!(good.warnings.is_empty());

    let SignatureVerdict::Good(forwarded) =
        verdict("alice", &alice, "bob@example.org", "2025-03-01")
    else {
        panic!("expected a good signature");
    };
    assert_eq!(forwarded.sender, SenderBinding::Mismatch);
}

#[test]
fn a_one_byte_change_makes_a_signature_bad() {
    let alice = [certificate("alice")];
    let mut tampered = signed_entity();
    let last = tampered.len() - 3;
    tampered[last] ^= 0x20;
    let verdicts = Verifier::new(&alice, "alice@example.org", at("2025-03-01"))
        .detached(&tampered, &gnupg_signature("alice"));
    assert_eq!(
        verdicts,
        [SignatureVerdict::Bad {
            signer: alice[0].id().clone()
        }]
    );
}

#[test]
fn nothing_a_compromised_key_signed_is_good_even_before_its_revocation() {
    let mallory = [certificate("revoked-compromised")];
    assert_eq!(
        verdict(
            "revoked-compromised",
            &mallory,
            "mallory@example.org",
            "2025-03-01"
        ),
        SignatureVerdict::Unusable {
            signer: mallory[0].id().clone(),
            reason: UnusableReason::Revoked,
        }
    );
}

#[test]
fn what_a_superseded_key_signed_before_its_revocation_stays_good() {
    let sam = [certificate("revoked-superseded")];
    assert!(matches!(
        verdict("revoked-superseded", &sam, "sam@example.org", "2025-07-01"),
        SignatureVerdict::Good(_)
    ));
}

#[test]
fn a_signature_made_before_its_key_expired_stays_good_after() {
    let eve = [certificate("expiring")];
    assert!(matches!(
        verdict("expiring", &eve, "eve@example.org", "2026-06-01"),
        SignatureVerdict::Good(_)
    ));
}

#[test]
fn an_rsa_2048_signature_is_good_with_a_warning() {
    let robin = [certificate("rsa2048")];
    let SignatureVerdict::Good(good) =
        verdict("rsa2048", &robin, "robin@example.org", "2025-03-01")
    else {
        panic!("expected a good signature");
    };
    assert!(good.warnings.contains(&Warning::WeakPublicKey {
        algorithm: AlgorithmName::new("RSA"),
        bits: 2048,
    }));
}

#[test]
fn an_rsa_1024_signature_is_unusable() {
    // RFC 9580 §12.4: MUST NOT verify using RSA keys under 2048 bits.
    let tiny = [certificate("rsa1024")];
    assert_eq!(
        verdict("rsa1024", &tiny, "tiny@example.org", "2025-03-01"),
        SignatureVerdict::Unusable {
            signer: tiny[0].id().clone(),
            reason: UnusableReason::RejectedAlgorithm,
        }
    );
}

#[test]
fn a_message_signature_on_sha1_never_validates() {
    // RFC 9580 §9.5, openpgp.md: mail is not in the reader's custody.
    let old = [certificate("sha1-self-signature-2020")];
    assert_eq!(
        verdict("sha1-message", &old, "old@example.org", "2020-07-01"),
        SignatureVerdict::Unusable {
            signer: old[0].id().clone(),
            reason: UnusableReason::RejectedAlgorithm,
        }
    );
}

#[test]
fn a_signer_without_a_certificate_is_unknown_and_named_by_fingerprint() {
    let alice = certificate("alice");
    let SignatureVerdict::UnknownSigner {
        issuer: Some(IssuerHint::Handle { octets, .. }),
    } = verdict(
        "alice",
        &[certificate("expiring")],
        "alice@example.org",
        "2025-03-01",
    )
    else {
        panic!("expected an unknown signer with a handle");
    };
    assert_eq!(&*octets, alice.id().fingerprint());
}

#[test]
fn a_part_with_no_signature_in_it_is_malformed() {
    let alice = [certificate("alice")];
    let verifier = Verifier::new(&alice, "alice@example.org", at("2025-03-01"));
    for part in [
        &b""[..],
        b"not a signature",
        b"-----BEGIN PGP SIGNATURE-----\n\n!!!\n-----END PGP SIGNATURE-----\n",
    ] {
        assert_eq!(
            verifier.detached(&signed_entity(), part),
            [SignatureVerdict::Malformed]
        );
    }
}
