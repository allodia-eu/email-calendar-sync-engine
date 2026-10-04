//! What opening a sealed layer records, and how a walk ends without one.

mod support;

use engine_core::e2e::{
    CertificateId, ChainStatus, DecryptionFailure, DecryptionOutcome, GoodSignature, LayerKind,
    SenderBinding, SignatureVerdict, Summary,
};
use engine_e2e::{Payload, SealReason, UnwrapFailure, Walk};
use support::{FAKE, crlf, fake_decryption, walk_with};

const ENCRYPTED: &str = "Content-Type: application/pkcs7-mime; smime-type=enveloped-data\n\
\n\
Content-Type: text/plain\n\
\n\
secret\n";

fn good_from_sender() -> SignatureVerdict {
    SignatureVerdict::Good(GoodSignature {
        signer: CertificateId::open_pgp(&[5; 32]).expect("v6"),
        created: "2026-09-25T10:00:00Z".parse().expect("time"),
        sender: SenderBinding::Matches,
        chain: ChainStatus::NotApplicable,
        warnings: Vec::new(),
    })
}

#[test]
fn a_signature_found_inside_the_encryption_makes_the_layer_combined() {
    let walked = walk_with(&crlf(ENCRYPTED), &[good_from_sender()]);
    let layer = &walked.envelope().layers()[0];
    assert_eq!(layer.kind(), LayerKind::SignedAndEncrypted);
    assert_eq!(layer.signatures(), [good_from_sender()]);
    assert_eq!(
        layer.decryption(),
        Some(&DecryptionOutcome::Decrypted(fake_decryption())),
    );
    assert_eq!(walked.summary(), Some(Summary::Confidential));
}

#[test]
fn stopping_at_a_sealed_layer_leaves_the_payload_sealed() {
    let Walk::Encrypted(layer) = engine_e2e::walk(&crlf(ENCRYPTED), FAKE) else {
        panic!("expected an encrypted layer");
    };
    assert_eq!(
        layer.object(),
        b"Content-Type: text/plain\r\n\r\nsecret\r\n"
    );
    let walked = layer.stop();
    assert_eq!(
        walked.envelope().payload(),
        &Payload::Sealed(SealReason::NotOpened)
    );
    assert_eq!(walked.envelope().layers()[0].kind(), LayerKind::Encrypted);
    assert_eq!(walked.envelope().layers()[0].decryption(), None);
    // Nothing can be said about a message whose payload was never reached.
    assert_eq!(walked.summary(), None);
    assert_eq!(walked.payload_bytes(), None);
}

#[test]
fn a_failed_decryption_is_recorded_and_releases_nothing() {
    let Walk::Encrypted(layer) = engine_e2e::walk(&crlf(ENCRYPTED), FAKE) else {
        panic!("expected an encrypted layer");
    };
    let Walk::Complete(walked) = layer.failed(DecryptionFailure::IntegrityFailure) else {
        panic!("a failure ends the walk");
    };
    assert_eq!(
        walked.envelope().payload(),
        &Payload::Sealed(SealReason::DecryptionFailed(
            DecryptionFailure::IntegrityFailure
        )),
    );
    assert_eq!(
        walked.envelope().layers()[0].decryption(),
        Some(&DecryptionOutcome::Failed(
            DecryptionFailure::IntegrityFailure
        )),
    );
    assert_eq!(walked.summary(), None);
}

#[test]
fn a_failed_unwrap_is_recorded() {
    let raw = ENCRYPTED.replace("enveloped-data", "signed-data");
    let Walk::EmbeddedSignature(layer) = engine_e2e::walk(&crlf(&raw), FAKE) else {
        panic!("expected an embedded signature");
    };
    let Walk::Complete(walked) = layer.failed(UnwrapFailure::Malformed) else {
        panic!("a failure ends the walk");
    };
    assert_eq!(
        walked.envelope().payload(),
        &Payload::Sealed(SealReason::UnwrapFailed(UnwrapFailure::Malformed)),
    );
}

#[test]
fn stopping_at_an_embedded_signature_leaves_the_payload_sealed() {
    let raw = ENCRYPTED.replace("enveloped-data", "signed-data");
    let Walk::EmbeddedSignature(layer) = engine_e2e::walk(&crlf(&raw), FAKE) else {
        panic!("expected an embedded signature");
    };
    assert_eq!(
        layer.stop().envelope().payload(),
        &Payload::Sealed(SealReason::NotOpened)
    );
}

#[test]
fn an_embedded_signature_carries_its_verdicts() {
    let raw = ENCRYPTED.replace("enveloped-data", "signed-data");
    let Walk::EmbeddedSignature(layer) = engine_e2e::walk(&crlf(&raw), FAKE) else {
        panic!("expected an embedded signature");
    };
    let entity = layer.object().to_vec();
    let Walk::Complete(walked) = layer.unwrapped(entity, vec![good_from_sender()]) else {
        panic!("a text payload ends the walk");
    };
    assert_eq!(
        walked.envelope().layers()[0].signatures(),
        [good_from_sender()]
    );
    assert_eq!(walked.summary(), Some(Summary::Verified));
}

#[test]
fn a_sealed_layer_names_its_mechanism() {
    let Walk::Encrypted(layer) = engine_e2e::walk(&crlf(ENCRYPTED), FAKE) else {
        panic!("expected an encrypted layer");
    };
    assert_eq!(layer.mechanism(), engine_core::e2e::Mechanism::OpenPgp);
}
