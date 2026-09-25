//! A fake mechanism: it recognises PGP/MIME and S/MIME structures by their
//! content types and does no cryptography, so every shape in RFC 9787 §4 can be
//! walked before any real mechanism exists.
//!
//! Its "ciphertext" is the protected MIME entity itself, carried verbatim as the
//! encrypted object's body, so "decrypting" is handing the object back.

#![allow(dead_code, unreachable_pub)]

use engine_core::e2e::{AlgorithmName, Decryption, Integrity, Mechanism, SignatureVerdict};
use engine_e2e::{
    InlineKind, LayerRecogniser, LayerShape, Object, PartView, Recognition, Walk, Walked,
};

/// Recognises layers by content type alone.
#[derive(Debug, Default)]
pub struct Fake;

impl LayerRecogniser for Fake {
    fn mechanism(&self) -> Mechanism {
        Mechanism::OpenPgp
    }

    fn recognise(&self, part: &PartView<'_>) -> Option<Recognition> {
        if part.is("multipart", "signed") {
            return Some(Recognition::Layer(LayerShape::DetachedSignature));
        }
        if part.is("multipart", "encrypted") {
            let children: Vec<_> = part.children().collect();
            let shaped = children.len() == 2
                && children[0].is("application", "pgp-encrypted")
                && children[1].is("application", "octet-stream");
            return Some(if shaped {
                Recognition::Layer(LayerShape::Encryption(Object::Child(1)))
            } else {
                Recognition::Malformed
            });
        }
        if part.is("application", "pkcs7-mime") {
            let smime_type = part.param("smime-type").map(str::to_ascii_lowercase);
            return match smime_type.as_deref() {
                Some("enveloped-data" | "authenveloped-data") => {
                    Some(Recognition::Layer(LayerShape::Encryption(Object::Body)))
                }
                Some("signed-data") => Some(Recognition::Layer(LayerShape::EmbeddedSignature(
                    Object::Body,
                ))),
                // RFC 9787 §4.1.1.5: compression is not a cryptographic layer.
                _ => None,
            };
        }
        None
    }

    fn inline(&self, text: &str) -> Option<InlineKind> {
        if text.contains("-----BEGIN FAKE MESSAGE-----") {
            Some(InlineKind::Encrypted)
        } else if text.contains("-----BEGIN FAKE SIGNED MESSAGE-----") {
            Some(InlineKind::Signed)
        } else {
            None
        }
    }
}

/// The registry most tests walk with.
pub const FAKE: &[&dyn LayerRecogniser] = &[&Fake];

/// Turns the `\n` line endings of a readable fixture into the CRLF that mail
/// carries on the wire.
pub fn crlf(text: &str) -> Vec<u8> {
    text.replace('\n', "\r\n").into_bytes()
}

/// What the fake reports for every decryption.
pub fn fake_decryption() -> Decryption {
    Decryption {
        integrity: Integrity::Authenticated,
        cipher: AlgorithmName::new("AES-256"),
        aead: Some(AlgorithmName::new("OCB")),
        warnings: Vec::new(),
    }
}

/// Walks `raw` to the end, opening every sealed layer by handing its object back,
/// with `inner` as the verdicts found inside each encrypted layer.
pub fn walk_with(raw: &[u8], inner: &[SignatureVerdict]) -> Walked {
    let mut walk = engine_e2e::walk(raw, FAKE);
    loop {
        walk = match walk {
            Walk::Complete(walked) => return walked,
            Walk::Encrypted(layer) => {
                let entity = layer.object().to_vec();
                layer.decrypted(entity, fake_decryption(), inner.to_vec())
            }
            Walk::EmbeddedSignature(layer) => {
                let entity = layer.object().to_vec();
                layer.unwrapped(entity, Vec::new())
            }
        };
    }
}

/// Walks `raw` to the end with no signatures inside encrypted layers.
pub fn walk_all(raw: &[u8]) -> Walked {
    walk_with(raw, &[])
}

/// Where a part sits: which source (0 is the message, then one per opened layer)
/// and the child indices from that source's root.
pub fn at(part: &engine_e2e::PartRef) -> (usize, Vec<usize>) {
    (part.source().index(), part.path().to_vec())
}
