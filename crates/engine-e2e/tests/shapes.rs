//! Every layer shape in RFC 9787 §4, walked through the fake mechanism.

mod support;

use engine_core::e2e::{LayerKind, SignatureForm, Summary};
use engine_e2e::Payload;
use support::{at, crlf, walk_all};

const DETACHED: LayerKind = LayerKind::Signed(SignatureForm::Detached);
const EMBEDDED: LayerKind = LayerKind::Signed(SignatureForm::Embedded);

fn kinds(walked: &engine_e2e::Walked) -> Vec<LayerKind> {
    walked
        .envelope()
        .layers()
        .iter()
        .map(engine_e2e::Layer::kind)
        .collect()
}

fn payload_at(walked: &engine_e2e::Walked) -> (usize, Vec<usize>) {
    match walked.envelope().payload() {
        Payload::Reached(part) => at(part),
        Payload::Sealed(reason) => panic!("payload sealed: {reason:?}"),
    }
}

/// An entity carried as an opaque object's body: what the fake "decrypts" to.
const INNER_TEXT: &str = "Content-Type: text/plain\n\nsecret\n";

fn pgp_encrypted(inner: &str) -> String {
    format!(
        "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"e\"\n\
         \n\
         --e\n\
         Content-Type: application/pgp-encrypted\n\
         \n\
         Version: 1\n\
         --e\n\
         Content-Type: application/octet-stream\n\
         \n\
         {inner}\n\
         --e--\n"
    )
}

fn smime(smime_type: &str, inner: &str) -> String {
    format!("Content-Type: application/pkcs7-mime; smime-type=\"{smime_type}\"\n\n{inner}")
}

const PGP_SIGNED: &str = "Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; micalg=pgp-sha256; boundary=\"s\"\n\
\n\
--s\n\
Content-Type: text/plain\n\
\n\
hello\n\
--s\n\
Content-Type: application/pgp-signature\n\
\n\
SIGNATURE\n\
--s--\n";

#[test]
fn a_message_that_is_not_a_layer_has_no_envelope() {
    let walked = walk_all(&crlf("Content-Type: text/plain\n\nhello\n"));
    assert!(kinds(&walked).is_empty());
    assert_eq!(payload_at(&walked), (0, vec![]));
    assert_eq!(walked.summary(), Some(Summary::Unprotected));
}

#[test]
fn pgp_mime_signing_layer() {
    // RFC 9787 §4.1.2.1.
    let walked = walk_all(&crlf(PGP_SIGNED));
    assert_eq!(kinds(&walked), [DETACHED]);
    assert_eq!(payload_at(&walked), (0, vec![0]));
    assert_eq!(
        walked.payload_bytes(),
        Some(&b"Content-Type: text/plain\r\n\r\nhello"[..]),
    );
    // Nothing verified the signature yet, so nothing is claimed for it.
    assert_eq!(walked.summary(), Some(Summary::Unprotected));
}

#[test]
fn pgp_mime_encryption_layer() {
    // RFC 9787 §4.1.2.2.
    let walked = walk_all(&crlf(&pgp_encrypted(INNER_TEXT)));
    assert_eq!(kinds(&walked), [LayerKind::Encrypted]);
    assert_eq!(payload_at(&walked), (1, vec![]));
    assert_eq!(
        walked.payload_bytes(),
        Some(&b"Content-Type: text/plain\r\n\r\nsecret\r\n"[..]),
    );
    assert_eq!(walked.summary(), Some(Summary::EncryptedButUnverified));
}

#[test]
fn s_mime_multipart_signed_layer() {
    // RFC 9787 §4.1.1.1.
    let raw = PGP_SIGNED
        .replace("application/pgp-signature", "application/pkcs7-signature")
        .replace("pgp-sha256", "sha-256");
    let walked = walk_all(&crlf(&raw));
    assert_eq!(kinds(&walked), [DETACHED]);
    assert_eq!(payload_at(&walked), (0, vec![0]));
}

#[test]
fn s_mime_signed_data_layer() {
    // RFC 9787 §4.1.1.2: the protected part is unwrapped from the object.
    let walked = walk_all(&crlf(&smime("signed-data", INNER_TEXT)));
    assert_eq!(kinds(&walked), [EMBEDDED]);
    assert_eq!(payload_at(&walked), (1, vec![]));
}

#[test]
fn s_mime_enveloped_and_auth_enveloped_data_layers() {
    // RFC 9787 §4.1.1.3 and §4.1.1.4: the same structure.
    for smime_type in ["enveloped-data", "authEnveloped-data"] {
        let walked = walk_all(&crlf(&smime(smime_type, INNER_TEXT)));
        assert_eq!(kinds(&walked), [LayerKind::Encrypted], "{smime_type}");
        assert_eq!(payload_at(&walked), (1, vec![]), "{smime_type}");
    }
}

#[test]
fn compression_is_not_a_layer() {
    // RFC 9787 §4.1.1.5.
    let walked = walk_all(&crlf(&smime("compressed-data", INNER_TEXT)));
    assert!(kinds(&walked).is_empty());
    assert_eq!(payload_at(&walked), (0, vec![]));
    assert!(walked.envelope().errant().is_empty());
}

#[test]
fn multilayer_envelope() {
    // RFC 9787 §4.4.2: A (enveloped-data) decrypts to C (signed-data), which
    // unwraps to E, the payload.
    let raw = smime("enveloped-data", &smime("signed-data", INNER_TEXT));
    let walked = walk_all(&crlf(&raw));
    assert_eq!(kinds(&walked), [LayerKind::Encrypted, EMBEDDED]);
    assert_eq!(payload_at(&walked), (2, vec![]));
    assert!(walked.envelope().errant().is_empty());
}

#[test]
fn encrypt_then_sign_keeps_the_order() {
    // The signature outside the encryption signed the ciphertext (RFC 9787 §5.2).
    let raw = PGP_SIGNED.replace(
        "Content-Type: text/plain\n\nhello\n",
        &smime("enveloped-data", INNER_TEXT),
    );
    let walked = walk_all(&crlf(&raw));
    assert_eq!(kinds(&walked), [DETACHED, LayerKind::Encrypted]);
    assert_eq!(payload_at(&walked), (1, vec![]));
}

const MAILING_LIST: &str = "Content-Type: multipart/mixed; boundary=\"h\"\n\
\n\
--h\n\
Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"i\"\n\
\n\
--i\n\
Content-Type: text/plain\n\
\n\
J\n\
--i\n\
Content-Type: application/pgp-signature\n\
\n\
K\n\
--i--\n\
\n\
--h\n\
Content-Type: text/plain\n\
\n\
L: the list's footer\n\
--h--\n";

#[test]
fn mailing_list_wrapping_has_no_envelope_and_one_errant_layer() {
    // RFC 9787 §4.5.1: H wraps I; I is errant.
    let walked = walk_all(&crlf(MAILING_LIST));
    assert!(kinds(&walked).is_empty());
    assert_eq!(payload_at(&walked), (0, vec![]));
    let errant = walked.envelope().errant();
    assert_eq!(errant.len(), 1);
    assert_eq!(at(errant[0].part()), (0, vec![0]));
    assert_eq!(errant[0].kind(), DETACHED);
    // An errant layer never contributes to the summary (RFC 9787 §4.5).
    assert_eq!(walked.summary(), Some(Summary::Unprotected));
}

#[test]
fn the_baroque_example() {
    // RFC 9787 §4.5.2: envelope M and Q, payload R, errant S.
    let r = MAILING_LIST
        .replace("J\n", "T\n")
        .replace("K\n", "U\n")
        .replace("L: the list's footer\n", "V\n");
    let q = PGP_SIGNED.replace("Content-Type: text/plain\n\nhello\n", &r);
    let walked = walk_all(&crlf(&pgp_encrypted(&q)));
    assert_eq!(kinds(&walked), [LayerKind::Encrypted, DETACHED]);
    assert_eq!(payload_at(&walked), (1, vec![0]));
    let errant = walked.envelope().errant();
    assert_eq!(errant.len(), 1);
    assert_eq!(at(errant[0].part()), (1, vec![0, 0]));
}

#[test]
fn a_forwarded_signed_message_is_not_an_errant_layer() {
    // An attached message/rfc822 is a message of its own, with its own envelope.
    let raw = format!(
        "Content-Type: multipart/mixed; boundary=\"f\"\n\
         \n\
         --f\n\
         Content-Type: text/plain\n\
         \n\
         see attached\n\
         --f\n\
         Content-Type: message/rfc822\n\
         \n\
         {PGP_SIGNED}\n\
         --f--\n"
    );
    let walked = walk_all(&crlf(&raw));
    assert!(kinds(&walked).is_empty());
    assert!(walked.envelope().errant().is_empty());
}

#[test]
fn a_layer_inside_an_errant_layer_is_not_listed_again() {
    // The errant subtree is rendered in isolation; only its root is errant here.
    let nested = MAILING_LIST.replace(
        "Content-Type: text/plain\n\nJ\n",
        &PGP_SIGNED.replace("\"s\"", "\"n\"").replace("--s", "--n"),
    );
    let walked = walk_all(&crlf(&nested));
    let errant = walked.envelope().errant();
    assert_eq!(errant.len(), 1);
    assert_eq!(at(errant[0].part()), (0, vec![0]));
}
