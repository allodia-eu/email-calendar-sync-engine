//! Which MIME parts are PGP/MIME layers (RFC 3156 §4, §5), and which text carries
//! OpenPGP inline (RFC 9580 §6.2.1).

use engine_core::e2e::Mechanism;
use engine_e2e::{InlineKind, LayerRecogniser, LayerShape, Object, PartView, Recognition};

/// Recognises PGP/MIME layers and inline OpenPGP, for `engine_e2e::walk`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PgpMime;

const SIGNATURE: &str = "pgp-signature";
const ENCRYPTED: &str = "pgp-encrypted";

impl LayerRecogniser for PgpMime {
    fn mechanism(&self) -> Mechanism {
        Mechanism::OpenPgp
    }

    fn recognise(&self, part: &PartView<'_>) -> Option<Recognition> {
        if part.is("multipart", "signed") && has_protocol(part, SIGNATURE) {
            // The walk checks there are exactly two parts; the second must be the
            // signature (RFC 3156 §5). `micalg` is not consulted: the signature
            // packet names its hash inside what is verified.
            let signature_labelled = part
                .child(1)
                .is_none_or(|signature| signature.is("application", SIGNATURE));
            return Some(if signature_labelled {
                Recognition::Layer(LayerShape::DetachedSignature)
            } else {
                Recognition::Malformed
            });
        }
        if part.is("multipart", "encrypted") && has_protocol(part, ENCRYPTED) {
            // Exactly two parts: the control part, then the ciphertext (RFC 3156 §4).
            let mut children = part.children();
            let shaped = match (children.next(), children.next(), children.next()) {
                (Some(control), Some(ciphertext), None) => {
                    control.is("application", ENCRYPTED)
                        && ciphertext.is("application", "octet-stream")
                }
                _ => false,
            };
            return Some(if shaped {
                Recognition::Layer(LayerShape::Encryption(Object::Child(1)))
            } else {
                Recognition::Malformed
            });
        }
        None
    }

    fn inline(&self, text: &str) -> Option<InlineKind> {
        // An armour header line starts its line and has only whitespace after it
        // (RFC 9580 §6.2.1), so a quoted one does not count.
        text.lines().find_map(|line| match line.trim_end() {
            "-----BEGIN PGP MESSAGE-----" => Some(InlineKind::Encrypted),
            "-----BEGIN PGP SIGNED MESSAGE-----" => Some(InlineKind::Signed),
            _ => None,
        })
    }
}

/// Whether the part's `protocol` is `application/<subtype>`, without regard to case.
fn has_protocol(part: &PartView<'_>, subtype: &str) -> bool {
    part.param("protocol").is_some_and(|protocol| {
        protocol.split_once('/').is_some_and(|(kind, sub)| {
            kind.trim().eq_ignore_ascii_case("application")
                && sub.trim().eq_ignore_ascii_case(subtype)
        })
    })
}
