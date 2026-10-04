//! Broken, deep and hostile structures: recorded, bounded, never a panic.

mod support;

use engine_core::e2e::{Mechanism, Summary};
use engine_e2e::{
    InlineKind, LayerRecogniser, LayerShape, MAX_LAYERS, Object, PartView, Payload, Recognition,
    SealReason, Walk,
};
use support::{at, crlf, walk_all};

fn signed(inner: &str, depth: usize) -> String {
    format!(
        "Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"b{depth}\"\n\
         \n\
         --b{depth}\n\
         {inner}\n\
         --b{depth}\n\
         Content-Type: application/pgp-signature\n\
         \n\
         SIG\n\
         --b{depth}--\n"
    )
}

fn nested_signed(depth: usize) -> String {
    (0..depth).fold(
        String::from("Content-Type: text/plain\n\nhi"),
        |inner, d| signed(&inner, d),
    )
}

fn nested_encrypted(depth: usize) -> String {
    (0..depth).fold(
        String::from("Content-Type: text/plain\n\nhi\n"),
        |inner, _| {
            format!("Content-Type: application/pkcs7-mime; smime-type=enveloped-data\n\n{inner}")
        },
    )
}

#[test]
fn layers_up_to_the_limit_are_walked() {
    let walked = walk_all(&crlf(&nested_signed(MAX_LAYERS)));
    assert_eq!(walked.envelope().layers().len(), MAX_LAYERS);
    assert!(matches!(walked.envelope().payload(), Payload::Reached(_)));
}

#[test]
fn signed_layers_past_the_limit_seal_the_payload() {
    let walked = walk_all(&crlf(&nested_signed(MAX_LAYERS + 1)));
    assert_eq!(walked.envelope().layers().len(), MAX_LAYERS);
    assert_eq!(
        walked.envelope().payload(),
        &Payload::Sealed(SealReason::TooDeep)
    );
    assert_eq!(walked.summary(), None);
}

#[test]
fn encrypted_layers_past_the_limit_seal_the_payload() {
    let walked = walk_all(&crlf(&nested_encrypted(MAX_LAYERS + 1)));
    assert_eq!(walked.envelope().layers().len(), MAX_LAYERS);
    assert_eq!(
        walked.envelope().payload(),
        &Payload::Sealed(SealReason::TooDeep)
    );
}

#[test]
fn a_multipart_signed_without_exactly_two_parts_is_malformed_not_a_layer() {
    // RFC 1847 §2.1: exactly two body parts.
    for body in [
        "--s\nContent-Type: text/plain\n\nonly\n--s--\n",
        "--s\nContent-Type: text/plain\n\none\n--s\n\ntwo\n--s\n\nthree\n--s--\n",
    ] {
        let raw = format!(
            "Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"s\"\n\n{body}"
        );
        let walked = walk_all(&crlf(&raw));
        assert!(walked.envelope().layers().is_empty(), "{body}");
        let malformed = walked.envelope().malformed();
        assert_eq!(malformed.len(), 1, "{body}");
        assert_eq!(at(malformed[0].part()), (0, vec![]));
        assert_eq!(
            walked.envelope().payload(),
            &Payload::Reached(malformed[0].part().clone())
        );
        assert_eq!(walked.summary(), Some(Summary::Unprotected));
    }
}

#[test]
fn a_misshapen_encryption_layer_is_malformed_not_a_layer() {
    // What a mail server that rewrites PGP/MIME leaves behind: the parts survive
    // under the wrong structure.
    let raw = "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"e\"\n\
               \n\
               --e\n\
               Content-Type: text/plain\n\
               \n\
               \n\
               --e\n\
               Content-Type: application/octet-stream\n\
               \n\
               CIPHERTEXT\n\
               --e--\n";
    let walked = walk_all(&crlf(raw));
    assert!(walked.envelope().layers().is_empty());
    assert_eq!(walked.envelope().malformed().len(), 1);
    assert_eq!(walked.summary(), Some(Summary::Unprotected));
}

#[test]
fn a_malformed_layer_inside_the_payload_is_recorded_and_its_contents_scanned() {
    let raw = "Content-Type: multipart/mixed; boundary=\"m\"\n\
               \n\
               --m\n\
               Content-Type: multipart/signed; boundary=\"s\"\n\
               \n\
               --s\n\
               Content-Type: text/plain\n\
               \n\
               -----BEGIN FAKE SIGNED MESSAGE-----\n\
               --s--\n\
               --m--\n";
    let walked = walk_all(&crlf(raw));
    let envelope = walked.envelope();
    assert!(envelope.errant().is_empty());
    assert_eq!(envelope.malformed().len(), 1);
    assert_eq!(at(envelope.malformed()[0].part()), (0, vec![0]));
    assert_eq!(envelope.inline().len(), 1);
    assert_eq!(at(envelope.inline()[0].part()), (0, vec![0, 0]));
}

/// Names a ciphertext child without checking it exists.
struct Careless;

impl LayerRecogniser for Careless {
    fn mechanism(&self) -> Mechanism {
        Mechanism::OpenPgp
    }

    fn recognise(&self, part: &PartView<'_>) -> Option<Recognition> {
        part.is("multipart", "encrypted")
            .then_some(Recognition::Layer(LayerShape::Encryption(Object::Child(5))))
    }

    fn inline(&self, _text: &str) -> Option<InlineKind> {
        None
    }
}

#[test]
fn an_object_child_that_does_not_exist_is_malformed() {
    let raw = "Content-Type: multipart/encrypted; boundary=\"e\"\n\
               \n\
               --e\n\
               \n\
               only\n\
               --e--\n";
    let Walk::Complete(walked) = engine_e2e::walk(&crlf(raw), &[&Careless]) else {
        panic!("a malformed layer is not opened");
    };
    assert!(walked.envelope().layers().is_empty());
    assert_eq!(walked.envelope().malformed().len(), 1);
}

#[test]
fn inline_protection_in_the_payload_is_found_not_walked() {
    // RFC 9787 §6.2.3: inline protection is outside the MIME structure.
    for (marker, kind) in [
        ("-----BEGIN FAKE MESSAGE-----", InlineKind::Encrypted),
        ("-----BEGIN FAKE SIGNED MESSAGE-----", InlineKind::Signed),
    ] {
        let raw = format!("Content-Type: text/plain\n\nabove\n{marker}\nbelow\n");
        let walked = walk_all(&crlf(&raw));
        let inline = walked.envelope().inline();
        assert_eq!(inline.len(), 1, "{marker}");
        assert_eq!(inline[0].kind(), kind);
        assert_eq!(at(inline[0].part()), (0, vec![]));
        assert!(walked.envelope().layers().is_empty());
        assert_eq!(walked.summary(), Some(Summary::Unprotected));
    }
}

#[test]
fn hostile_input_never_panics() {
    let mut corpus: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"\r\n\r\n".to_vec(),
        b"Content-Type: multipart/signed\r\n\r\n".to_vec(),
        b"Content-Type: multipart/signed; boundary=\"\"\r\n\r\n----\r\n".to_vec(),
        b"Content-Type: application/pkcs7-mime; smime-type=enveloped-data\r\n\r\n".to_vec(),
        b"Content-Type: multipart/encrypted; boundary=e\r\n\r\n--e\r\n--e\r\n--e--".to_vec(),
        vec![0xff; 64],
    ];
    // Every prefix of real shapes: truncation anywhere.
    for shape in [nested_signed(3), nested_encrypted(3)] {
        let bytes = crlf(&shape);
        corpus.extend((0..bytes.len()).map(|end| bytes[..end].to_vec()));
    }
    // A deterministic byte soup.
    let alphabet = b"-\r\n:;=\"ab/ multipartsignedencryptedContent-Type";
    let mut state = 0x2545_f491_u32;
    for _ in 0..200 {
        let soup = (0..128)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                alphabet[state as usize % alphabet.len()]
            })
            .collect();
        corpus.push(soup);
    }
    for raw in &corpus {
        let walked = walk_all(raw);
        let _ = walked.summary();
        let _ = walked.payload_bytes();
        let _ = walked.detached_signatures();
    }
}
