//! What each finding reports, and what a walk never prints.

mod support;

use engine_core::e2e::Mechanism;
use engine_e2e::{PartView, Walk};
use support::{FAKE, crlf, walk_all};

/// A payload with one of everything the scan records: an errant layer, a malformed
/// layer, and inline protection.
const EVERYTHING: &str = "Content-Type: multipart/mixed; boundary=\"m\"\n\
\n\
--m\n\
Content-Type: multipart/signed; boundary=\"s\"\n\
\n\
--s\n\
Content-Type: text/plain\n\
\n\
signed\n\
--s\n\
Content-Type: application/pgp-signature\n\
\n\
SIG\n\
--s--\n\
--m\n\
Content-Type: multipart/encrypted; boundary=\"e\"\n\
\n\
--e\n\
Content-Type: text/plain\n\
\n\
not a control part\n\
--e--\n\
--m\n\
Content-Type: text/plain\n\
\n\
-----BEGIN FAKE MESSAGE-----\n\
--m--\n";

#[test]
fn every_finding_names_its_mechanism_and_part() {
    let walked = walk_all(&crlf(EVERYTHING));
    let envelope = walked.envelope();
    assert_eq!(envelope.errant()[0].mechanism(), Mechanism::OpenPgp);
    assert_eq!(envelope.errant()[0].part().path(), [0]);
    // A part the recogniser itself calls malformed, inside the payload.
    assert_eq!(envelope.malformed()[0].mechanism(), Mechanism::OpenPgp);
    assert_eq!(envelope.malformed()[0].part().path(), [1]);
    assert_eq!(envelope.inline()[0].mechanism(), Mechanism::OpenPgp);
    assert_eq!(envelope.inline()[0].part().path(), [2]);
    assert_eq!(
        walked.part_bytes(envelope.inline()[0].part()),
        Some(&b"Content-Type: text/plain\r\n\r\n-----BEGIN FAKE MESSAGE-----"[..]),
    );
}

#[test]
fn a_layer_names_its_mechanism() {
    let walked = walk_all(&crlf(
        "Content-Type: multipart/signed; boundary=\"s\"\n\n--s\n\nx\n--s\n\nSIG\n--s--\n",
    ));
    assert_eq!(
        walked.envelope().layers()[0].mechanism(),
        Mechanism::OpenPgp
    );
}

#[test]
fn an_embedded_signature_names_its_mechanism() {
    let raw = crlf("Content-Type: application/pkcs7-mime; smime-type=signed-data\n\nx");
    let Walk::EmbeddedSignature(layer) = engine_e2e::walk(&raw, FAKE) else {
        panic!("expected an embedded signature");
    };
    assert_eq!(layer.mechanism(), Mechanism::OpenPgp);
}

#[test]
fn a_part_from_another_walk_has_no_bytes_here() {
    let encrypted = walk_all(&crlf(
        "Content-Type: application/pkcs7-mime; smime-type=enveloped-data\n\n\
         Content-Type: text/plain\n\nsecret\n",
    ));
    let plain = walk_all(&crlf("Content-Type: text/plain\n\nhello\n"));
    let engine_e2e::Payload::Reached(opened) = encrypted.envelope().payload() else {
        panic!("reached");
    };
    assert_eq!(opened.source().index(), 1);
    assert_eq!(plain.part_bytes(opened), None);
}

fn sealed(smime_type: &str) -> Vec<u8> {
    crlf(&format!(
        "Content-Type: application/pkcs7-mime; smime-type={smime_type}\n\n\
         Content-Type: text/plain\n\nsecret words\n"
    ))
}

#[test]
fn debug_output_never_shows_content() {
    let Walk::Encrypted(layer) = engine_e2e::walk(&sealed("enveloped-data"), FAKE) else {
        panic!("expected an encrypted layer");
    };
    let shown = format!("{layer:?}");
    assert!(shown.contains("EncryptedLayer"), "{shown}");
    assert!(!shown.contains("secret"), "{shown}");

    let Walk::EmbeddedSignature(layer) = engine_e2e::walk(&sealed("signed-data"), FAKE) else {
        panic!("expected an embedded signature");
    };
    let shown = format!("{layer:?}");
    assert!(shown.contains("EmbeddedSignatureLayer"), "{shown}");
    assert!(!shown.contains("secret"), "{shown}");

    let walked = walk_all(&sealed("enveloped-data"));
    assert!(walked.payload_bytes().is_some());
    let shown = format!("{:?}", Walk::Complete(walked));
    assert!(shown.contains("source_lengths"), "{shown}");
    assert!(!shown.contains("secret"), "{shown}");
}

/// Records what a recogniser's `Debug` of a part shows.
struct Peek(std::cell::RefCell<Vec<String>>);

impl engine_e2e::LayerRecogniser for Peek {
    fn mechanism(&self) -> Mechanism {
        Mechanism::OpenPgp
    }

    fn recognise(&self, part: &PartView<'_>) -> Option<engine_e2e::Recognition> {
        self.0.borrow_mut().push(format!("{part:?}"));
        None
    }

    fn inline(&self, _text: &str) -> Option<engine_e2e::InlineKind> {
        None
    }
}

#[test]
fn a_part_view_shows_its_type_not_its_content() {
    let peek = Peek(std::cell::RefCell::new(Vec::new()));
    let _ = engine_e2e::walk(&crlf("Content-Type: text/plain\n\nsecret\n"), &[&peek]);
    let shown = peek.0.borrow().join(" ");
    assert!(shown.contains("plain"), "{shown}");
    assert!(!shown.contains("secret"), "{shown}");
}
