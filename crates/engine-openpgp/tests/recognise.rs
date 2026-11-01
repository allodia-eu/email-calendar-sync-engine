//! PGP/MIME recognition (RFC 3156 §4, §5) and inline detection (RFC 9580 §6.2.1).

use std::fmt::Write as _;

use engine_core::e2e::{LayerKind, Mechanism, SignatureForm};
use engine_e2e::{InlineKind, LayerRecogniser, Payload, Walk, Walked};
use engine_openpgp::PgpMime;

const RECOGNISERS: &[&dyn LayerRecogniser] = &[&PgpMime];

fn crlf(text: &str) -> Vec<u8> {
    text.replace('\n', "\r\n").into_bytes()
}

fn walk(text: &str) -> Walk<'static> {
    engine_e2e::walk(&crlf(text), RECOGNISERS)
}

fn complete(text: &str) -> Walked {
    match walk(text) {
        Walk::Complete(walked) => walked,
        other => panic!("expected the walk to complete, got {other:?}"),
    }
}

fn signed(content_type: &str, signature_type: &str) -> String {
    format!(
        "Content-Type: {content_type}\n\
         \n\
         --s\n\
         Content-Type: text/plain\n\
         \n\
         hello\n\
         --s\n\
         Content-Type: {signature_type}\n\
         \n\
         -----BEGIN PGP SIGNATURE-----\n\
         -----END PGP SIGNATURE-----\n\
         --s--\n"
    )
}

fn encrypted(content_type: &str, parts: &[(&str, &str)]) -> String {
    let mut body = format!("Content-Type: {content_type}\n\n");
    for (part_type, content) in parts {
        write!(body, "--e\nContent-Type: {part_type}\n\n{content}\n").expect("a String grows");
    }
    body + "--e--\n"
}

const PGP_SIGNED: &str =
    "multipart/signed; boundary=\"s\"; micalg=pgp-sha256; protocol=\"application/pgp-signature\"";
const PGP_ENCRYPTED: &str =
    "multipart/encrypted; boundary=\"e\"; protocol=\"application/pgp-encrypted\"";
const CONTROL: (&str, &str) = ("application/pgp-encrypted", "Version: 1");
const CIPHERTEXT: (&str, &str) = (
    "application/octet-stream",
    "-----BEGIN PGP MESSAGE-----\n\nCIPHER\n-----END PGP MESSAGE-----",
);

#[test]
fn a_signing_layer_is_recognised_by_its_protocol() {
    for content_type in [
        PGP_SIGNED.to_owned(),
        // Parameter values compare without regard to case.
        PGP_SIGNED.replace("application/pgp-signature", "Application/PGP-Signature"),
    ] {
        let walked = complete(&signed(&content_type, "application/pgp-signature"));
        let layers = walked.envelope().layers();
        assert_eq!(layers.len(), 1, "{content_type}");
        assert_eq!(layers[0].kind(), LayerKind::Signed(SignatureForm::Detached));
        assert_eq!(layers[0].mechanism(), Mechanism::OpenPgp);
        assert!(matches!(walked.envelope().payload(), Payload::Reached(_)));
    }
}

#[test]
fn a_multipart_signed_for_another_protocol_is_not_ours() {
    for content_type in [
        PGP_SIGNED.replace("application/pgp-signature", "application/pkcs7-signature"),
        "multipart/signed; boundary=\"s\"; micalg=pgp-sha256".to_owned(),
    ] {
        let walked = complete(&signed(&content_type, "application/pgp-signature"));
        assert!(walked.envelope().layers().is_empty(), "{content_type}");
        assert!(walked.envelope().malformed().is_empty(), "{content_type}");
    }
}

#[test]
fn a_signing_layer_whose_signature_part_is_mislabelled_is_malformed() {
    for signature_type in ["text/plain", "application/pkcs7-signature"] {
        let walked = complete(&signed(PGP_SIGNED, signature_type));
        assert!(walked.envelope().layers().is_empty(), "{signature_type}");
        assert_eq!(walked.envelope().malformed().len(), 1, "{signature_type}");
    }
}

#[test]
fn micalg_does_not_decide_recognition() {
    for content_type in [
        PGP_SIGNED.replace("micalg=pgp-sha256; ", ""),
        PGP_SIGNED.replace("pgp-sha256", "pgp-md5"),
        PGP_SIGNED.replace("pgp-sha256", "nonsense"),
    ] {
        let walked = complete(&signed(&content_type, "application/pgp-signature"));
        assert_eq!(walked.envelope().layers().len(), 1, "{content_type}");
    }
}

#[test]
fn an_encryption_layer_is_recognised_by_its_protocol() {
    for content_type in [
        PGP_ENCRYPTED.to_owned(),
        PGP_ENCRYPTED.replace("application/pgp-encrypted", "APPLICATION/PGP-ENCRYPTED"),
    ] {
        let Walk::Encrypted(layer) = walk(&encrypted(&content_type, &[CONTROL, CIPHERTEXT])) else {
            panic!("expected an encryption layer for {content_type}");
        };
        assert_eq!(layer.mechanism(), Mechanism::OpenPgp);
        // The object is the second part's body, transfer-decoded.
        assert_eq!(
            layer.object(),
            b"-----BEGIN PGP MESSAGE-----\r\n\r\nCIPHER\r\n-----END PGP MESSAGE-----",
        );
    }
}

#[test]
fn a_multipart_encrypted_for_another_protocol_is_not_ours() {
    for content_type in [
        PGP_ENCRYPTED.replace("application/pgp-encrypted", "application/x-other"),
        "multipart/encrypted; boundary=\"e\"".to_owned(),
    ] {
        let walked = complete(&encrypted(&content_type, &[CONTROL, CIPHERTEXT]));
        assert!(walked.envelope().layers().is_empty(), "{content_type}");
        assert!(walked.envelope().malformed().is_empty(), "{content_type}");
    }
}

#[test]
fn an_encryption_layer_with_the_wrong_structure_is_malformed() {
    let wrong_control = ("text/plain", "Version: 1");
    let wrong_object = ("text/plain", "CIPHER");
    for parts in [
        vec![CIPHERTEXT, CONTROL],
        vec![wrong_control, CIPHERTEXT],
        vec![CONTROL, wrong_object],
        vec![CONTROL],
        vec![CONTROL, CIPHERTEXT, CIPHERTEXT],
    ] {
        let walked = complete(&encrypted(PGP_ENCRYPTED, &parts));
        assert!(walked.envelope().layers().is_empty(), "{parts:?}");
        assert_eq!(walked.envelope().malformed().len(), 1, "{parts:?}");
    }
}

#[test]
fn inline_protection_is_found_at_the_start_of_a_line() {
    for (armour, kind) in [
        ("-----BEGIN PGP MESSAGE-----", InlineKind::Encrypted),
        ("-----BEGIN PGP SIGNED MESSAGE-----", InlineKind::Signed),
        // Only whitespace may follow the header line (RFC 9580 §6.2.1).
        ("-----BEGIN PGP MESSAGE-----  \t", InlineKind::Encrypted),
    ] {
        let walked = complete(&format!(
            "Content-Type: text/plain\n\nabove\n{armour}\nbelow\n"
        ));
        let inline = walked.envelope().inline();
        assert_eq!(inline.len(), 1, "{armour}");
        assert_eq!(inline[0].kind(), kind);
        assert_eq!(inline[0].mechanism(), Mechanism::OpenPgp);
    }
}

#[test]
fn armour_not_at_the_start_of_a_line_is_not_inline_protection() {
    for text in [
        "> -----BEGIN PGP MESSAGE-----",
        "see -----BEGIN PGP MESSAGE----- above",
        "-----BEGIN PGP MESSAGE----- trailing",
        "-----BEGIN PGP PUBLIC KEY BLOCK-----",
        "-----begin pgp message-----",
    ] {
        let walked = complete(&format!("Content-Type: text/plain\n\n{text}\n"));
        assert!(walked.envelope().inline().is_empty(), "{text}");
    }
}
