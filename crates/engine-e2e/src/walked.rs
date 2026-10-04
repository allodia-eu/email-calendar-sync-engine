//! A finished walk: the envelope, the bytes it points into, and the detached
//! signatures still to check.

use core::fmt;
use std::borrow::Cow;

use engine_core::e2e::{LayerSignatures, SignatureVerdict, Summary};

use crate::envelope::{Envelope, LayerIndex, PartRef, Payload};

/// The result of a walk.
///
/// It owns the message and every entity an opened layer produced, so it holds
/// decrypted content: its `Debug` shows lengths, never bytes.
pub struct Walked {
    sources: Vec<Vec<u8>>,
    envelope: Envelope,
}

impl Walked {
    pub(crate) fn new(sources: Vec<Vec<u8>>, envelope: Envelope) -> Self {
        Self { sources, envelope }
    }

    /// Returns the message's cryptographic structure.
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    /// Returns the bytes of `part`, headers included.
    ///
    /// Returns `None` for a part this walk did not produce.
    pub fn part_bytes(&self, part: &PartRef) -> Option<&[u8]> {
        self.sources.get(part.source().index())?.get(part.range())
    }

    /// Returns the payload's bytes, a MIME entity headers included, when the walk
    /// reached it.
    pub fn payload_bytes(&self) -> Option<&[u8]> {
        match &self.envelope.payload {
            Payload::Reached(part) => self.part_bytes(part),
            Payload::Sealed(_) => None,
        }
    }

    /// Returns the envelope's detached signatures, outermost first, each with the
    /// exact bytes it covers.
    pub fn detached_signatures(&self) -> Vec<DetachedSignature<'_>> {
        self.envelope
            .layers
            .iter()
            .enumerate()
            .filter_map(|(index, layer)| {
                let detached = layer.detached.as_ref()?;
                let source = self.sources.get(layer.part().source().index())?;
                Some(DetachedSignature {
                    layer: LayerIndex(index),
                    signed: canonical(source.get(detached.signed.clone())?),
                    signature: &detached.signature,
                })
            })
            .collect()
    }

    /// Records the verdicts on a detached signature's check, replacing any recorded
    /// before. An index from another walk that names no layer here is ignored.
    pub fn record_verdicts(&mut self, layer: LayerIndex, verdicts: Vec<SignatureVerdict>) {
        if let Some(layer) = self.envelope.layers.get_mut(layer.0) {
            layer.signatures = verdicts;
        }
    }

    /// Returns the message's cryptographic summary, from the envelope alone.
    ///
    /// Returns `None` when the payload was not reached: whether a sealed message is
    /// Confidential depends on a signature nobody has seen yet.
    pub fn summary(&self) -> Option<Summary> {
        if let Payload::Sealed(_) = self.envelope.payload {
            return None;
        }
        let layers: Vec<_> = self
            .envelope
            .layers
            .iter()
            .map(|layer| LayerSignatures {
                kind: layer.kind(),
                verdicts: layer.signatures(),
            })
            .collect();
        Some(Summary::derive(&layers))
    }
}

impl fmt::Debug for Walked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lengths: Vec<_> = self.sources.iter().map(Vec::len).collect();
        f.debug_struct("Walked")
            .field("source_lengths", &lengths)
            .field("layers", &self.envelope.layers.len())
            .field("payload", &self.envelope.payload)
            .finish_non_exhaustive()
    }
}

/// A detached signature and the bytes it signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedSignature<'w> {
    layer: LayerIndex,
    signed: Cow<'w, [u8]>,
    signature: &'w [u8],
}

impl<'w> DetachedSignature<'w> {
    /// Returns the layer the signature belongs to, for
    /// [`Walked::record_verdicts`].
    pub fn layer(&self) -> LayerIndex {
        self.layer
    }

    /// Returns the signed part, headers included, exactly as the signature covers
    /// it: without the line break that belongs to the boundary after it (RFC 2046
    /// §5.1.1), and with every line ending CRLF (RFC 3156 §5).
    pub fn signed(&self) -> Cow<'w, [u8]> {
        self.signed.clone()
    }

    /// Returns the signature part's body, content-transfer-decoded.
    pub fn signature(&self) -> &'w [u8] {
        self.signature
    }
}

/// Converts every bare LF to CRLF, leaving existing CRLF and lone CR alone.
///
/// Mail that crossed a system storing LF-only text arrives that way; the signer
/// signed CRLF (RFC 3156 §5), so verification must see CRLF again.
fn canonical(bytes: &[u8]) -> Cow<'_, [u8]> {
    let bare = |index: usize| bytes[index] == b'\n' && (index == 0 || bytes[index - 1] != b'\r');
    if !(0..bytes.len()).any(bare) {
        return Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 32);
    for (index, &byte) in bytes.iter().enumerate() {
        if bare(index) {
            out.push(b'\r');
        }
        out.push(byte);
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::canonical;

    #[test]
    fn canonical_leaves_crlf_text_borrowed() {
        assert!(matches!(
            canonical(b"a\r\nb\r\n"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn canonical_converts_only_bare_line_feeds() {
        assert_eq!(&*canonical(b"\na\r\nb\nc\r"), b"\r\na\r\nb\r\nc\r");
    }
}
