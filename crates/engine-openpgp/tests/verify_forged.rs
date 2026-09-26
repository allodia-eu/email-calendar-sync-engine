//! Verdicts no tool produces on request, forged exactly (RFC 9580 §5.2.3.36,
//! §5.2.3.18, §5.2.5).

mod support;

use engine_core::e2e::{SenderBinding, SignatureVerdict, UnusableReason};
use engine_openpgp::{Certificate, SignatureContext, Verifier};
use pgp::{
    composed::DetachedSignature,
    packet::{Signature, SignatureType},
    ser::Serialize,
    types::{KeyDetails, KeyVersion},
};
use support::{
    DAY, at,
    forge::{Cert, Forge, Primary, Sig, SubKind, user_id},
    secs,
};

const DATA: &[u8] = b"Content-Type: text/plain\r\n\r\nforged\r\n";
const CERTIFY_SIGN: u8 = 0x03;

/// A v6 certificate for alice@example.org, signing with its primary key.
fn alice(forge: &mut Forge) -> (Primary, Certificate) {
    let key = forge.primary(KeyVersion::V6, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let direct = forge.direct(&key, Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN));
    let certification = forge.certify(&key, &alice, Sig::at(secs("2025-01-01")));
    let bytes = Cert::new(&key)
        .direct(direct)
        .user(alice, vec![certification])
        .bytes();
    (key, Certificate::parse(&bytes).expect("parses"))
}

fn part(signatures: &[Signature]) -> Vec<u8> {
    signatures
        .iter()
        .flat_map(|signature| {
            DetachedSignature::new(signature.clone())
                .to_bytes()
                .expect("serialize")
        })
        .collect()
}

fn check(certificates: &[Certificate], signature: &Signature) -> SignatureVerdict {
    let verdicts = Verifier::new(certificates, "alice@example.org", at("2025-03-01"))
        .detached(DATA, &part(std::slice::from_ref(signature)));
    assert_eq!(verdicts.len(), 1);
    verdicts.into_iter().next().expect("one verdict")
}

#[test]
fn a_version_6_signature_is_good() {
    let mut forge = Forge::new();
    let (key, cert) = alice(&mut forge);
    for typ in [SignatureType::Binary, SignatureType::Text] {
        let signature = forge.sign(&key, typ, Sig::at(secs("2025-02-01")), DATA);
        let SignatureVerdict::Good(good) = check(std::slice::from_ref(&cert), &signature) else {
            panic!("expected a good {typ:?} signature");
        };
        assert_eq!(good.sender, SenderBinding::Matches);
    }
}

#[test]
fn intended_recipients_admit_a_signature_only_inside_their_encryption() {
    // RFC 9580 §5.2.3.36: valid "only in an encrypted context, where the key it was
    // encrypted to is one of the indicated primary keys".
    let mut forge = Forge::new();
    let (key, cert) = alice(&mut forge);
    let signature = forge.sign(
        &key,
        SignatureType::Binary,
        Sig::at(secs("2025-02-01")).intended_for(key.public.fingerprint()),
        DATA,
    );
    let signed = part(std::slice::from_ref(&signature));
    let certificates = std::slice::from_ref(&cert);
    let verifier = Verifier::new(certificates, "alice@example.org", at("2025-03-01"));
    let not_for_us = SignatureVerdict::NotForThisRecipient {
        signer: cert.id().clone(),
    };
    // A signature forwarded out of its encryption.
    assert_eq!(
        verifier.detached(DATA, &signed),
        std::slice::from_ref(&not_for_us)
    );
    // Inside encryption opened with the named certificate.
    let ours = verifier.within(SignatureContext::DecryptedWith(cert.id()));
    assert!(matches!(
        ours.detached(DATA, &signed)[..],
        [SignatureVerdict::Good(_)]
    ));
    // Inside encryption to someone else.
    let (_, other) = alice(&mut forge);
    let theirs = verifier.within(SignatureContext::DecryptedWith(other.id()));
    assert_eq!(theirs.detached(DATA, &signed), [not_for_us]);
}

#[test]
fn a_subkey_not_bound_for_signing_signs_nothing_good() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let sub = forge.subkey(&key, SubKind::Sign, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let certification = forge.certify(&key, &alice, Sig::at(secs("2025-01-01")).flags(0x01));
    // Bound for encryption only: a signing algorithm is not a signing key.
    let binding = forge.bind(&key, &sub, Sig::at(secs("2025-01-01")).flags(0x04));
    let cert = Certificate::parse(
        &Cert::new(&key)
            .user(alice, vec![certification])
            .subkey(&sub, vec![binding])
            .bytes(),
    )
    .expect("parses");
    let signature = forge.sign_with_subkey(&sub, Sig::at(secs("2025-02-01")), DATA);
    assert_eq!(
        check(std::slice::from_ref(&cert), &signature),
        SignatureVerdict::Unusable {
            signer: cert.id().clone(),
            reason: UnusableReason::NotSigningCapable,
        }
    );
}

#[test]
fn a_signature_past_its_own_expiration_is_unusable() {
    let mut forge = Forge::new();
    let (key, cert) = alice(&mut forge);
    let lasting = forge.sign(
        &key,
        SignatureType::Binary,
        Sig::at(secs("2025-02-01")).signature_expires(100 * DAY),
        DATA,
    );
    assert!(matches!(
        check(std::slice::from_ref(&cert), &lasting),
        SignatureVerdict::Good(_)
    ));
    let short = forge.sign(
        &key,
        SignatureType::Binary,
        Sig::at(secs("2025-02-01")).signature_expires(DAY),
        DATA,
    );
    assert_eq!(
        check(std::slice::from_ref(&cert), &short),
        SignatureVerdict::Unusable {
            signer: cert.id().clone(),
            reason: UnusableReason::Expired,
        }
    );
}

#[test]
fn a_signature_made_after_its_key_expired_is_unusable() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V6, secs("2025-01-01"));
    let direct = forge.direct(
        &key,
        Sig::at(secs("2025-01-01"))
            .flags(CERTIFY_SIGN)
            .expires(10 * DAY),
    );
    let cert = Certificate::parse(&Cert::new(&key).direct(direct).bytes()).expect("parses");
    let signature = forge.sign(
        &key,
        SignatureType::Binary,
        Sig::at(secs("2025-02-01")),
        DATA,
    );
    assert_eq!(
        check(std::slice::from_ref(&cert), &signature),
        SignatureVerdict::Unusable {
            signer: cert.id().clone(),
            reason: UnusableReason::Expired,
        }
    );
}

#[test]
fn a_key_with_no_valid_self_signature_signs_nothing_good() {
    // A v6 key without its Direct Key signature (RFC 9580 §5.2.3.10).
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V6, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let certification = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN),
    );
    let cert = Certificate::parse(&Cert::new(&key).user(alice, vec![certification]).bytes())
        .expect("parses");
    let signature = forge.sign(
        &key,
        SignatureType::Binary,
        Sig::at(secs("2025-02-01")),
        DATA,
    );
    assert_eq!(
        check(std::slice::from_ref(&cert), &signature),
        SignatureVerdict::Unusable {
            signer: cert.id().clone(),
            reason: UnusableReason::NotSigningCapable,
        }
    );
}

#[test]
fn a_signature_older_than_its_key_is_malformed() {
    let mut forge = Forge::new();
    let (key, cert) = alice(&mut forge);
    let signature = forge.sign(
        &key,
        SignatureType::Binary,
        Sig::at(secs("2024-06-01")),
        DATA,
    );
    assert_eq!(
        check(std::slice::from_ref(&cert), &signature),
        SignatureVerdict::Malformed
    );
}

#[test]
fn a_certification_is_not_a_message_signature() {
    let mut forge = Forge::new();
    let (key, cert) = alice(&mut forge);
    let certification = forge.certify(&key, &user_id("x@example.org"), Sig::at(secs("2025-02-01")));
    assert_eq!(
        check(std::slice::from_ref(&cert), &certification),
        SignatureVerdict::Malformed
    );
}

#[test]
fn every_signature_in_a_part_gets_its_own_verdict() {
    // RFC 9580 §5.2.5: an unknown signature never stops the others.
    let mut forge = Forge::new();
    let (key, cert) = alice(&mut forge);
    let (stranger, _) = alice(&mut forge);
    let good = forge.sign(
        &key,
        SignatureType::Binary,
        Sig::at(secs("2025-02-01")),
        DATA,
    );
    let unknown = forge.sign(
        &stranger,
        SignatureType::Binary,
        Sig::at(secs("2025-02-01")),
        DATA,
    );
    let verdicts = Verifier::new(
        std::slice::from_ref(&cert),
        "alice@example.org",
        at("2025-03-01"),
    )
    .detached(DATA, &part(&[unknown, good]));
    assert!(matches!(
        verdicts[..],
        [
            SignatureVerdict::UnknownSigner { .. },
            SignatureVerdict::Good(_)
        ]
    ));
}

/// A version 4 signature packet built by hand, around the given hashed subpackets.
fn hand_built(version: u8, hashed: &[u8]) -> Vec<u8> {
    let mut body = vec![version, 0x00, 22, 8];
    body.extend(u16::try_from(hashed.len()).expect("short").to_be_bytes());
    body.extend(hashed);
    body.extend([0, 0, 0xAB, 0xCD]);
    body.extend([0x00, 0x08, 0x01, 0x00, 0x08, 0x01]);
    let mut packet = vec![0xC2, u8::try_from(body.len()).expect("short")];
    packet.extend(body);
    packet
}

fn created_2025() -> Vec<u8> {
    let mut subpacket = vec![5, 2];
    subpacket.extend(secs("2025-02-01").to_be_bytes());
    subpacket
}

#[test]
fn a_hand_built_signature_parses() {
    // The control for the two below: without the flaw, the same packet reaches the
    // signer lookup, so their Malformed is the flaw's and not the parser's.
    let verdicts = Verifier::new(&[], "alice@example.org", at("2025-03-01"))
        .detached(DATA, &hand_built(4, &created_2025()));
    assert!(matches!(
        verdicts[..],
        [SignatureVerdict::UnknownSigner { issuer: None }]
    ));
}

#[test]
fn a_signature_without_a_creation_time_is_malformed() {
    // RFC 9580 §5.2.3.11: the subpacket MUST be present in the hashed area.
    let verdicts = Verifier::new(&[], "alice@example.org", at("2025-03-01"))
        .detached(DATA, &hand_built(4, &[]));
    assert_eq!(verdicts, [SignatureVerdict::Malformed]);
}

#[test]
fn an_unknown_critical_subpacket_makes_a_signature_malformed() {
    // RFC 9580 §5.2.3.7.
    let mut hashed = created_2025();
    hashed.extend([2, 0x80 | 0x5A, 0]);
    let verdicts = Verifier::new(&[], "alice@example.org", at("2025-03-01"))
        .detached(DATA, &hand_built(4, &hashed));
    assert_eq!(verdicts, [SignatureVerdict::Malformed]);
}

#[test]
fn an_issuer_fingerprint_of_another_version_makes_a_signature_malformed() {
    // RFC 9580 §5.2.3.35: a v4 signature naming a v6 fingerprint.
    let mut hashed = created_2025();
    hashed.extend([34, 33, 6]);
    hashed.extend([0x11; 32]);
    let verdicts = Verifier::new(&[], "alice@example.org", at("2025-03-01"))
        .detached(DATA, &hand_built(4, &hashed));
    assert_eq!(verdicts, [SignatureVerdict::Malformed]);
}

#[test]
fn a_signature_version_this_engine_does_not_read_is_unsupported() {
    let verdicts = Verifier::new(&[], "alice@example.org", at("2025-03-01"))
        .detached(DATA, &hand_built(5, &created_2025()));
    assert_eq!(verdicts, [SignatureVerdict::Unsupported]);
}
