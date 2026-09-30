//! What a walk found: the envelope, its payload, and what lies outside it.

use core::ops::Range;

use engine_core::e2e::{
    DecryptionFailure, DecryptionOutcome, LayerKind, Mechanism, SignatureVerdict,
};

use crate::InlineKind;

/// Which bytes a part lives in: the message itself, or an entity an opened layer
/// produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub(crate) usize);

impl SourceId {
    /// The message the walk started from.
    pub const MESSAGE: Self = Self(0);

    /// Returns the position: 0 is the message, then one per opened layer, in the
    /// order they were opened.
    pub fn index(self) -> usize {
        self.0
    }
}

/// Where a part sits: its source, and the child indices that lead to it from that
/// source's root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartRef {
    source: SourceId,
    path: Box<[usize]>,
    range: Range<usize>,
}

impl PartRef {
    pub(crate) fn new(source: SourceId, path: &[usize], range: Range<usize>) -> Self {
        Self {
            source,
            path: path.into(),
            range,
        }
    }

    /// Returns the source the part lives in.
    pub fn source(&self) -> SourceId {
        self.source
    }

    /// Returns the child indices from the source's root; empty for the root itself.
    pub fn path(&self) -> &[usize] {
        &self.path
    }

    pub(crate) fn range(&self) -> Range<usize> {
        self.range.clone()
    }
}

/// Identifies one layer of a walked envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayerIndex(pub(crate) usize);

/// One cryptographic layer of the envelope (RFC 9787 §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    pub(crate) mechanism: Mechanism,
    pub(crate) kind: LayerKind,
    pub(crate) part: PartRef,
    pub(crate) signatures: Vec<SignatureVerdict>,
    pub(crate) decryption: Option<DecryptionOutcome>,
    pub(crate) detached: Option<Detached>,
}

/// A detached signature's two halves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Detached {
    /// The signed part's bytes within the layer's source.
    pub(crate) signed: Range<usize>,
    /// The signature part's body, content-transfer-decoded.
    pub(crate) signature: Vec<u8>,
}

impl Layer {
    pub(crate) fn new(mechanism: Mechanism, kind: LayerKind, part: PartRef) -> Self {
        Self {
            mechanism,
            kind,
            part,
            signatures: Vec::new(),
            decryption: None,
            detached: None,
        }
    }

    /// Returns the mechanism that recognised the layer.
    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }

    /// Returns what the layer does.
    pub fn kind(&self) -> LayerKind {
        self.kind
    }

    /// Returns the part the layer is rooted in.
    pub fn part(&self) -> &PartRef {
        &self.part
    }

    /// Returns the verdicts on the layer's signatures: the ones found when it was
    /// opened, or the ones recorded for a detached signature. Empty until then.
    pub fn signatures(&self) -> &[SignatureVerdict] {
        &self.signatures
    }

    /// Returns what opening the layer found, for an encryption layer someone tried to
    /// open.
    pub fn decryption(&self) -> Option<&DecryptionOutcome> {
        self.decryption.as_ref()
    }
}

/// Why an embedded signature did not unwrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnwrapFailure {
    /// The signature object could not be parsed.
    Malformed,
    /// The signature object uses a version or an algorithm this engine does not
    /// implement.
    Unsupported,
}

/// Why the walk did not reach the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SealReason {
    /// The caller stopped at a sealed layer without opening it.
    NotOpened,
    /// The innermost layer did not decrypt.
    DecryptionFailed(DecryptionFailure),
    /// The innermost layer did not unwrap.
    UnwrapFailed(UnwrapFailure),
    /// The envelope has more layers than [`MAX_LAYERS`](crate::MAX_LAYERS).
    TooDeep,
}

/// The envelope's payload: the first part inside it that is not a layer (RFC 9787
/// §4.3), or why it was not reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// The payload was reached. Without an envelope, it is the message itself.
    Reached(PartRef),
    /// The payload is still behind a layer.
    Sealed(SealReason),
}

/// A layer found inside the payload rather than around it (RFC 9787 §4.5). It never
/// contributes to the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrantLayer {
    pub(crate) mechanism: Mechanism,
    pub(crate) kind: LayerKind,
    pub(crate) part: PartRef,
}

impl ErrantLayer {
    /// Returns the mechanism that recognised the layer.
    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }

    /// Returns what the layer does, as far as its structure says.
    pub fn kind(&self) -> LayerKind {
        self.kind
    }

    /// Returns the part the layer is rooted in.
    pub fn part(&self) -> &PartRef {
        &self.part
    }
}

/// Protection found inline in a text part of the payload (RFC 9787 §6.2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineFinding {
    pub(crate) mechanism: Mechanism,
    pub(crate) kind: InlineKind,
    pub(crate) part: PartRef,
}

impl InlineFinding {
    /// Returns the mechanism whose protection this is.
    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }

    /// Returns what kind of inline protection it is.
    pub fn kind(&self) -> InlineKind {
        self.kind
    }

    /// Returns the text part that carries it.
    pub fn part(&self) -> &PartRef {
        &self.part
    }
}

/// A part that announced itself as a layer and broke that layer's structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedLayer {
    pub(crate) mechanism: Mechanism,
    pub(crate) part: PartRef,
}

impl MalformedLayer {
    /// Returns the mechanism whose layer the part claimed to be.
    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }

    /// Returns the part.
    pub fn part(&self) -> &PartRef {
        &self.part
    }
}

/// A message's cryptographic structure (RFC 9787 §4.2 to §4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub(crate) layers: Vec<Layer>,
    pub(crate) payload: Payload,
    pub(crate) errant: Vec<ErrantLayer>,
    pub(crate) inline: Vec<InlineFinding>,
    pub(crate) malformed: Vec<MalformedLayer>,
}

impl Envelope {
    /// Returns the envelope's layers, outermost first; empty when the message has no
    /// envelope.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Returns the payload.
    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    /// Returns the layers found inside the payload, in document order. Layers inside
    /// an errant layer are not listed: its subtree is shown in isolation.
    pub fn errant(&self) -> &[ErrantLayer] {
        &self.errant
    }

    /// Returns the inline protection found in the payload's text parts.
    pub fn inline(&self) -> &[InlineFinding] {
        &self.inline
    }

    /// Returns the parts that claimed to be layers and were not well formed, around
    /// the payload or inside it.
    pub fn malformed(&self) -> &[MalformedLayer] {
        &self.malformed
    }
}
