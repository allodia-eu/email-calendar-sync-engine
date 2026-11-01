//! Which subkey may do what, and when (RFC 9580 §5.2.1.8, §5.2.3.29, §10.1.5).

mod support;

use engine_openpgp::{Certificate, EncryptionPurpose};
use pgp::{packet::RevocationCode, types::KeyVersion};
use support::{
    DAY, at,
    forge::{Cert, Forge, Primary, Sig, Sub, SubKind, user_id},
    secs,
};

const CERTIFY: u8 = 0x01;
const SIGN: u8 = 0x02;
const ENCRYPT_COMMUNICATIONS: u8 = 0x04;
const ENCRYPT_STORAGE: u8 = 0x08;

/// A v4 certificate whose primary only certifies, carrying `sub` bound by
/// `signatures`.
fn with_subkey(
    forge: &mut Forge,
    key: &Primary,
    sub: &Sub,
    signatures: Vec<pgp::packet::Signature>,
) -> Certificate {
    let alice = user_id("Alice <alice@example.org>");
    let certification = forge.certify(key, &alice, Sig::at(secs("2025-01-01")).flags(CERTIFY));
    Certificate::parse(
        &Cert::new(key)
            .user(alice, vec![certification])
            .subkey(sub, signatures)
            .bytes(),
    )
    .expect("parses")
}

fn setup(kind: SubKind) -> (Forge, Primary, Sub) {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let sub = forge.subkey(&key, kind, secs("2025-01-01"));
    (forge, key, sub)
}

#[test]
fn a_signing_subkey_needs_its_back_signature() {
    // RFC 9580 §5.2.1.8: a binding for a signing subkey MUST embed a 0x19
    // signature made by the subkey.
    let (mut forge, key, sub) = setup(SubKind::Sign);
    let bare = forge.bind(&key, &sub, Sig::at(secs("2025-01-01")).flags(SIGN));
    let cert = with_subkey(&mut forge, &key, &sub, vec![bare]);
    assert!(cert.at(at("2025-03-01")).signing_keys().is_empty());

    let back = forge.back_signature(&key, &sub, secs("2025-01-01"));
    let bound = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(SIGN).embedded(back),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![bound]);
    let signing = cert.at(at("2025-03-01")).signing_keys();
    assert_eq!(signing.len(), 1);
    assert!(!signing[0].is_primary());
    assert!(cert.at(at("2025-03-01")).facts().can_sign);
}

#[test]
fn a_back_signature_from_another_key_does_not_count() {
    let (mut forge, key, sub) = setup(SubKind::Sign);
    let other = forge.subkey(&key, SubKind::Sign, secs("2025-01-01"));
    let wrong = forge.back_signature(&key, &other, secs("2025-01-01"));
    let bound = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(SIGN).embedded(wrong),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![bound]);
    assert!(cert.at(at("2025-03-01")).signing_keys().is_empty());
}

#[test]
fn encryption_to_others_needs_the_communications_flag() {
    let (mut forge, key, sub) = setup(SubKind::Encrypt);
    let storage_only = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(ENCRYPT_STORAGE),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![storage_only]);
    let view = cert.at(at("2025-03-01"));
    // openpgp.md: others need 0x04; a draft to oneself takes 0x04 or 0x08.
    assert!(view.encryption_keys(EncryptionPurpose::ToOthers).is_empty());
    assert_eq!(
        view.encryption_keys(EncryptionPurpose::ToSelfStorage).len(),
        1
    );
    assert!(!view.facts().can_encrypt);

    let both = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(ENCRYPT_COMMUNICATIONS | ENCRYPT_STORAGE),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![both]);
    let view = cert.at(at("2025-03-01"));
    assert_eq!(view.encryption_keys(EncryptionPurpose::ToOthers).len(), 1);
    assert_eq!(
        view.encryption_keys(EncryptionPurpose::ToSelfStorage).len(),
        1
    );
    assert!(view.facts().can_encrypt);
}

#[test]
fn a_subkey_expires_counting_from_its_own_creation() {
    let mut forge = Forge::new();
    let key = forge.primary(KeyVersion::V4, secs("2025-01-01"));
    let sub = forge.subkey(&key, SubKind::Encrypt, secs("2025-02-01"));
    let binding = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-02-01"))
            .flags(ENCRYPT_COMMUNICATIONS)
            .expires(30 * DAY),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![binding]);
    let keys = cert
        .at(at("2025-02-15"))
        .encryption_keys(EncryptionPurpose::ToOthers);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].expires(), Some(at("2025-03-03")));
    assert!(
        cert.at(at("2025-03-03"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .is_empty()
    );
}

#[test]
fn a_subkey_is_not_usable_before_its_binding() {
    let (mut forge, key, sub) = setup(SubKind::Encrypt);
    let binding = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-02-01")).flags(ENCRYPT_COMMUNICATIONS),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![binding]);
    assert!(
        cert.at(at("2025-01-15"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .is_empty()
    );
    assert_eq!(
        cert.at(at("2025-02-15"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .len(),
        1
    );
}

#[test]
fn a_hard_subkey_revocation_reaches_back() {
    let (mut forge, key, sub) = setup(SubKind::Encrypt);
    let binding = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(ENCRYPT_COMMUNICATIONS),
    );
    let revocation = forge.revoke_subkey(
        &key,
        &sub,
        Sig::at(secs("2025-06-01")).reason(RevocationCode::KeyCompromised),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![binding, revocation]);
    assert!(
        cert.at(at("2025-03-01"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .is_empty()
    );
}

#[test]
fn a_soft_subkey_revocation_applies_from_its_date() {
    let (mut forge, key, sub) = setup(SubKind::Encrypt);
    let binding = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(ENCRYPT_COMMUNICATIONS),
    );
    let revocation = forge.revoke_subkey(
        &key,
        &sub,
        Sig::at(secs("2025-06-01")).reason(RevocationCode::KeySuperseded),
    );
    let cert = with_subkey(&mut forge, &key, &sub, vec![binding, revocation]);
    assert_eq!(
        cert.at(at("2025-03-01"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .len(),
        1
    );
    assert!(
        cert.at(at("2025-07-01"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .is_empty()
    );
}

#[test]
fn a_revocation_without_a_reason_is_hard() {
    // openpgp.md: a missing reason cannot be assumed benign.
    let (mut forge, key, sub) = setup(SubKind::Encrypt);
    let binding = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(ENCRYPT_COMMUNICATIONS),
    );
    let revocation = forge.revoke_subkey(&key, &sub, Sig::at(secs("2025-06-01")));
    let cert = with_subkey(&mut forge, &key, &sub, vec![binding, revocation]);
    assert!(
        cert.at(at("2025-03-01"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .is_empty()
    );
}

#[test]
fn a_revoked_primary_key_offers_no_subkeys() {
    let (mut forge, key, sub) = setup(SubKind::Encrypt);
    let alice = user_id("Alice <alice@example.org>");
    let certification = forge.certify(&key, &alice, Sig::at(secs("2025-01-01")).flags(CERTIFY));
    let binding = forge.bind(
        &key,
        &sub,
        Sig::at(secs("2025-01-01")).flags(ENCRYPT_COMMUNICATIONS),
    );
    let revocation = forge.key_revocation(
        &key,
        Sig::at(secs("2025-06-01")).reason(RevocationCode::KeyRetired),
    );
    let cert = Certificate::parse(
        &Cert::new(&key)
            .revocation(revocation)
            .user(alice, vec![certification])
            .subkey(&sub, vec![binding])
            .bytes(),
    )
    .expect("parses");
    assert_eq!(
        cert.at(at("2025-03-01"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .len(),
        1
    );
    assert!(
        cert.at(at("2025-07-01"))
            .encryption_keys(EncryptionPurpose::ToOthers)
            .is_empty()
    );
}
