//! Which self-signature speaks for a key (RFC 9580 §5.2.3.10), forged exactly.

mod support;

use engine_core::e2e::{CertificateProblem, CertificateStatus, SenderBinding};
use engine_openpgp::{Certificate, EncryptionPurpose};
use pgp::types::KeyVersion;
use support::{
    DAY, at,
    forge::{Cert, Forge, Sig, user_id},
    secs,
};

const CERTIFY_SIGN: u8 = 0x03;

fn parse(bytes: &[u8]) -> Certificate {
    Certificate::parse(bytes).expect("a forged certificate parses")
}

#[test]
fn the_most_recent_valid_self_signature_wins() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let first = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN),
    );
    let second = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-02-01"))
            .flags(CERTIFY_SIGN)
            .expires(60 * DAY),
    );
    let cert = parse(&Cert::new(&key).user(alice, vec![first, second]).bytes());
    // Before the second exists, the first speaks: no expiry.
    assert_eq!(cert.at(at("2025-01-15")).facts().expires, None);
    // After, the second does: expiry counts from the key's creation.
    let facts = cert.at(at("2025-02-15")).facts();
    assert_eq!(facts.expires, Some(at("2025-03-02")));
    assert_eq!(
        cert.at(at("2025-03-02")).status(),
        CertificateStatus::Expired
    );
}

#[test]
fn a_newer_self_signature_that_does_not_verify_is_ignored() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let good = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN),
    );
    // Made over another User ID, then attached to this one: it cannot verify.
    let stray = forge.certify(
        &key,
        &user_id("Mallory <mallory@example.org>"),
        Sig::at(secs("2025-02-01")).flags(CERTIFY_SIGN).expires(DAY),
    );
    let cert = parse(&Cert::new(&key).user(alice, vec![good, stray]).bytes());
    let facts = cert.at(at("2025-03-01")).facts();
    assert_eq!(facts.status, CertificateStatus::Usable);
    assert_eq!(facts.expires, None);
}

#[test]
fn a_self_signature_expires_with_its_signature_expiration_time() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let short = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01"))
            .flags(CERTIFY_SIGN)
            .signature_expires(10 * DAY),
    );
    let cert = parse(&Cert::new(&key).user(alice, vec![short]).bytes());
    assert_eq!(
        cert.at(at("2025-01-05")).status(),
        CertificateStatus::Usable
    );
    assert_eq!(
        cert.at(at("2025-01-20")).status(),
        CertificateStatus::Unusable(CertificateProblem::NoValidSelfSignature)
    );
}

#[test]
fn a_version_6_key_needs_a_direct_key_signature() {
    // RFC 9580 §5.2.3.10: "An implementation MUST ensure that a valid Direct Key
    // signature is present before using a version 6 key".
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V6, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let certification = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN),
    );
    let without = parse(
        &Cert::new(&key)
            .user(alice.clone(), vec![certification.clone()])
            .bytes(),
    );
    assert_eq!(
        without.at(at("2025-03-01")).status(),
        CertificateStatus::Unusable(CertificateProblem::NoValidSelfSignature)
    );

    let direct = forge.direct(&key, Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN));
    let with = parse(
        &Cert::new(&key)
            .direct(direct)
            .user(alice, vec![certification])
            .bytes(),
    );
    let view = with.at(at("2025-03-01"));
    assert_eq!(view.status(), CertificateStatus::Usable);
    assert_eq!(view.binds("alice@example.org"), SenderBinding::Matches);
}

#[test]
fn a_version_6_key_takes_its_flags_and_expiry_from_the_direct_key_signature() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V6, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    // The User ID signature claims more; for v6 it is not where key facts live.
    let certification = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN).expires(DAY),
    );
    let direct = forge.direct(&key, Sig::at(secs("2025-01-01")).flags(0x01));
    let view_cert = parse(
        &Cert::new(&key)
            .direct(direct)
            .user(alice, vec![certification])
            .bytes(),
    );
    let facts = view_cert.at(at("2025-03-01")).facts();
    assert_eq!(facts.status, CertificateStatus::Usable);
    assert_eq!(facts.expires, None);
    assert!(
        !facts.can_sign,
        "the Direct Key signature grants certify only"
    );
}

#[test]
fn a_version_4_key_prefers_its_primary_user_id() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let work = user_id("Alice <alice@example.org>");
    let home = user_id("Alice <alice@example.net>");
    // The primary User ID is older, and says the key expires; the newer one does not.
    let primary = forge.certify(
        &key,
        &work,
        Sig::at(secs("2025-01-01"))
            .flags(CERTIFY_SIGN)
            .expires(100 * DAY)
            .primary(),
    );
    let other = forge.certify(&key, &home, Sig::at(secs("2025-02-01")).flags(CERTIFY_SIGN));
    let cert = parse(
        &Cert::new(&key)
            .user(home, vec![other])
            .user(work, vec![primary])
            .bytes(),
    );
    let facts = cert.at(at("2025-03-01")).facts();
    assert_eq!(facts.expires, Some(at("2025-04-11")));
    assert_eq!(facts.addresses, ["alice@example.net", "alice@example.org"]);
}

#[test]
fn a_version_4_key_without_user_ids_falls_back_to_a_direct_key_signature() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let direct = forge.direct(&key, Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN));
    let view_cert = parse(&Cert::new(&key).direct(direct).bytes());
    let facts = view_cert.at(at("2025-03-01")).facts();
    assert_eq!(facts.status, CertificateStatus::Usable);
    assert!(facts.can_sign);
}

#[test]
fn key_flags_are_read_from_the_hashed_area_only() {
    // RFC 9580 §13.13: an unhashed subpacket has no provenance.
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let signature = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01"))
            .flags(0x01)
            .unhashed_flags(CERTIFY_SIGN),
    );
    let cert = parse(&Cert::new(&key).user(alice, vec![signature]).bytes());
    let view = cert.at(at("2025-03-01"));
    assert_eq!(view.status(), CertificateStatus::Usable);
    assert!(view.signing_keys().is_empty());
}

#[test]
fn no_key_flags_means_no_usage() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let signature = forge.certify(&key, &alice, Sig::at(secs("2025-01-01")));
    let cert = parse(&Cert::new(&key).user(alice, vec![signature]).bytes());
    let view = cert.at(at("2025-03-01"));
    assert!(view.signing_keys().is_empty());
    assert!(view.encryption_keys(EncryptionPurpose::ToOthers).is_empty());
    assert!(!view.facts().can_sign);
}

#[test]
fn a_revoked_user_id_no_longer_binds_its_address() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let old = user_id("Alice <alice@example.net>");
    let current = user_id("Alice <alice@example.org>");
    let old_certification =
        forge.certify(&key, &old, Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN));
    let revocation = forge.revoke_user_id(&key, &old, Sig::at(secs("2025-03-01")));
    let current_certification = forge.certify(
        &key,
        &current,
        Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN).primary(),
    );
    let cert = parse(
        &Cert::new(&key)
            .user(old, vec![old_certification, revocation])
            .user(current, vec![current_certification])
            .bytes(),
    );
    assert_eq!(
        cert.at(at("2025-02-01")).binds("alice@example.net"),
        SenderBinding::Matches
    );
    let later = cert.at(at("2025-04-01"));
    assert_eq!(later.binds("alice@example.net"), SenderBinding::Mismatch);
    assert_eq!(later.facts().addresses, ["alice@example.org"]);
}

#[test]
fn a_user_id_certified_again_after_its_revocation_binds_again() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let alice = user_id("Alice <alice@example.org>");
    let first = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-01-01")).flags(CERTIFY_SIGN),
    );
    let revocation = forge.revoke_user_id(&key, &alice, Sig::at(secs("2025-02-01")));
    let again = forge.certify(
        &key,
        &alice,
        Sig::at(secs("2025-03-01")).flags(CERTIFY_SIGN),
    );
    let cert = parse(
        &Cert::new(&key)
            .user(alice, vec![first, revocation, again])
            .bytes(),
    );
    assert_eq!(
        cert.at(at("2025-02-15")).binds("alice@example.org"),
        SenderBinding::NoAddress
    );
    assert_eq!(
        cert.at(at("2025-03-15")).binds("alice@example.org"),
        SenderBinding::Matches
    );
}
