use super::*;
use crate::e2e::{CertificateId, ChainStatus, GoodSignature};

const DETACHED: LayerKind = LayerKind::Signed(SignatureForm::Detached);
const EMBEDDED: LayerKind = LayerKind::Signed(SignatureForm::Embedded);

fn good(sender: SenderBinding) -> SignatureVerdict {
    SignatureVerdict::Good(GoodSignature {
        signer: CertificateId::open_pgp(&[9; 20]).expect("v4"),
        created: "2026-09-25T10:00:00Z".parse().expect("time"),
        sender,
        chain: ChainStatus::NotApplicable,
        warnings: Vec::new(),
    })
}

fn layer(kind: LayerKind, verdicts: &[SignatureVerdict]) -> LayerSignatures<'_> {
    LayerSignatures { kind, verdicts }
}

#[test]
fn no_envelope_is_unprotected() {
    assert_eq!(Summary::derive(&[]), Summary::Unprotected);
}

#[test]
fn a_good_signature_from_the_sender_is_verified() {
    let verdicts = [good(SenderBinding::Matches)];
    assert_eq!(
        Summary::derive(&[layer(DETACHED, &verdicts)]),
        Summary::Verified
    );
}

#[test]
fn a_cleartext_message_with_a_bad_signature_is_unprotected() {
    // RFC 9787 §3.1 and §6.4.
    let verdicts = [SignatureVerdict::Bad {
        signer: CertificateId::open_pgp(&[9; 20]).expect("v4"),
    }];
    assert_eq!(
        Summary::derive(&[layer(DETACHED, &verdicts)]),
        Summary::Unprotected
    );
}

#[test]
fn an_unchecked_signature_is_unprotected() {
    assert_eq!(
        Summary::derive(&[layer(DETACHED, &[])]),
        Summary::Unprotected
    );
}

#[test]
fn a_good_signature_from_someone_else_does_not_verify_the_sender() {
    for binding in [SenderBinding::Mismatch, SenderBinding::NoAddress] {
        let verdicts = [good(binding)];
        assert_eq!(
            Summary::derive(&[layer(DETACHED, &verdicts)]),
            Summary::Unprotected,
            "{binding:?}",
        );
    }
}

#[test]
fn one_good_signature_among_failures_is_enough() {
    // RFC 9580 §5.2.5: a failing signature never stops the others.
    let verdicts = [SignatureVerdict::Malformed, good(SenderBinding::Matches)];
    assert_eq!(
        Summary::derive(&[layer(DETACHED, &verdicts)]),
        Summary::Verified
    );
}

#[test]
fn encryption_without_a_signature_is_encrypted_but_unverified() {
    assert_eq!(
        Summary::derive(&[layer(LayerKind::Encrypted, &[])]),
        Summary::EncryptedButUnverified
    );
}

#[test]
fn a_signature_inside_the_encryption_is_confidential() {
    let verdicts = [good(SenderBinding::Matches)];
    // PGP/MIME's combined form: one layer, the signature inside it.
    assert_eq!(
        Summary::derive(&[layer(LayerKind::SignedAndEncrypted, &verdicts)]),
        Summary::Confidential
    );
    // RFC 9787 §4.4.2: enveloped-data around signed-data.
    assert_eq!(
        Summary::derive(&[layer(LayerKind::Encrypted, &[]), layer(EMBEDDED, &verdicts)]),
        Summary::Confidential
    );
}

#[test]
fn a_signature_outside_the_encryption_does_not_count() {
    // RFC 9787 §5.2: encrypt-then-sign signs the ciphertext, not the content.
    let verdicts = [good(SenderBinding::Matches)];
    assert_eq!(
        Summary::derive(&[layer(DETACHED, &verdicts), layer(LayerKind::Encrypted, &[])]),
        Summary::EncryptedButUnverified
    );
}

#[test]
fn a_signature_between_two_encryptions_does_not_count() {
    let verdicts = [good(SenderBinding::Matches)];
    assert_eq!(
        Summary::derive(&[
            layer(LayerKind::Encrypted, &[]),
            layer(DETACHED, &verdicts),
            layer(LayerKind::Encrypted, &[]),
        ]),
        Summary::EncryptedButUnverified
    );
}

#[test]
fn a_bad_inner_signature_leaves_encryption_unverified() {
    let verdicts = [SignatureVerdict::Unsupported];
    assert_eq!(
        Summary::derive(&[layer(LayerKind::SignedAndEncrypted, &verdicts)]),
        Summary::EncryptedButUnverified
    );
}

#[test]
fn only_encrypting_kinds_encrypt() {
    assert!(LayerKind::Encrypted.encrypts());
    assert!(LayerKind::SignedAndEncrypted.encrypts());
    assert!(!DETACHED.encrypts());
    assert!(!EMBEDDED.encrypts());
}
