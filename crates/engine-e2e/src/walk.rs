//! The layer walk (RFC 9787 §4): from the root inwards, one layer at a time.
//!
//! The walk never decrypts. When it meets a layer whose protected part is sealed
//! inside an object, it hands the caller that object and waits: the caller opens it
//! with whatever keys it holds (or stops), and the walk continues on what came
//! out. That keeps this crate free of cryptography, and lets the caller open a
//! layer however it must, synchronously or not.

use core::fmt;

use engine_core::e2e::{
    Decryption, DecryptionFailure, DecryptionOutcome, LayerKind, Mechanism, SignatureVerdict,
};
use mail_parser::{Message, MessageParser};

use crate::{
    LayerRecogniser, Walked,
    envelope::{
        Detached, Envelope, Layer, MalformedLayer, PartRef, Payload, SealReason, SourceId,
        UnwrapFailure,
    },
    recognise::{LayerShape, Object, PartView, Recognition, recognise},
    scan::{Findings, scan},
};

/// The most layers an envelope may have before the walk refuses to go further.
///
/// RFC 9787 sets no limit, and nothing legitimate comes close: an ordinary
/// protected message has one or two layers. The limit is what stops a hostile
/// message from nesting without end.
pub const MAX_LAYERS: usize = 8;

/// Starts walking the message `raw`, with `recognisers` asked about each part in
/// turn.
pub fn walk<'r>(raw: &[u8], recognisers: &'r [&'r dyn LayerRecogniser]) -> Walk<'r> {
    State {
        recognisers,
        sources: vec![raw.to_vec()],
        layers: Vec::new(),
        malformed: Vec::new(),
    }
    .descend(SourceId::MESSAGE)
}

/// Where a walk stands.
#[derive(Debug)]
pub enum Walk<'r> {
    /// The walk is over: the payload was reached, or cannot be.
    Complete(Walked),
    /// The next layer is encrypted; open it to go on.
    Encrypted(EncryptedLayer<'r>),
    /// The next layer wraps its protected part in a signature object; unwrap it to
    /// go on.
    EmbeddedSignature(EmbeddedSignatureLayer<'r>),
}

/// An encrypted layer the walk is waiting on.
pub struct EncryptedLayer<'r> {
    state: State<'r>,
    layer: Layer,
    object: Vec<u8>,
}

impl<'r> EncryptedLayer<'r> {
    /// Returns the mechanism that recognised the layer.
    pub fn mechanism(&self) -> Mechanism {
        self.layer.mechanism
    }

    /// Returns the encrypted object, content-transfer-decoded.
    pub fn object(&self) -> &[u8] {
        &self.object
    }

    /// Continues the walk into the decrypted `entity`, a MIME entity, recording how
    /// it was decrypted and the verdicts on any signatures found inside it.
    pub fn decrypted(
        mut self,
        entity: Vec<u8>,
        decryption: Decryption,
        signatures: Vec<SignatureVerdict>,
    ) -> Walk<'r> {
        self.layer.decryption = Some(DecryptionOutcome::Decrypted(decryption));
        if !signatures.is_empty() {
            self.layer.kind = LayerKind::SignedAndEncrypted;
        }
        self.layer.signatures = signatures;
        self.state.open(self.layer, entity)
    }

    /// Ends the walk: the layer did not decrypt.
    pub fn failed(mut self, failure: DecryptionFailure) -> Walk<'r> {
        self.layer.decryption = Some(DecryptionOutcome::Failed(failure));
        self.state.layers.push(self.layer);
        let payload = Payload::Sealed(SealReason::DecryptionFailed(failure));
        Walk::Complete(self.state.finish(payload, Findings::default()))
    }

    /// Ends the walk here, without opening the layer.
    pub fn stop(mut self) -> Walked {
        self.state.layers.push(self.layer);
        let payload = Payload::Sealed(SealReason::NotOpened);
        self.state.finish(payload, Findings::default())
    }
}

/// An embedded signature the walk is waiting on.
pub struct EmbeddedSignatureLayer<'r> {
    state: State<'r>,
    layer: Layer,
    object: Vec<u8>,
}

impl<'r> EmbeddedSignatureLayer<'r> {
    /// Returns the mechanism that recognised the layer.
    pub fn mechanism(&self) -> Mechanism {
        self.layer.mechanism
    }

    /// Returns the signature object, content-transfer-decoded.
    pub fn object(&self) -> &[u8] {
        &self.object
    }

    /// Continues the walk into the unwrapped `entity`, a MIME entity, recording the
    /// verdicts on the object's signatures.
    pub fn unwrapped(mut self, entity: Vec<u8>, signatures: Vec<SignatureVerdict>) -> Walk<'r> {
        self.layer.signatures = signatures;
        self.state.open(self.layer, entity)
    }

    /// Ends the walk: the object did not unwrap.
    pub fn failed(mut self, failure: UnwrapFailure) -> Walk<'r> {
        self.state.layers.push(self.layer);
        let payload = Payload::Sealed(SealReason::UnwrapFailed(failure));
        Walk::Complete(self.state.finish(payload, Findings::default()))
    }

    /// Ends the walk here, without unwrapping the layer.
    pub fn stop(mut self) -> Walked {
        self.state.layers.push(self.layer);
        let payload = Payload::Sealed(SealReason::NotOpened);
        self.state.finish(payload, Findings::default())
    }
}

/// Everything a paused walk carries. The sources hold decrypted content, so no
/// `Debug` of it shows them.
struct State<'r> {
    recognisers: &'r [&'r dyn LayerRecogniser],
    sources: Vec<Vec<u8>>,
    layers: Vec<Layer>,
    malformed: Vec<MalformedLayer>,
}

/// How walking one source ended.
enum Step {
    Reached(PartRef, Findings),
    TooDeep,
    Sealed(Layer, Vec<u8>),
}

impl<'r> State<'r> {
    fn open(mut self, layer: Layer, entity: Vec<u8>) -> Walk<'r> {
        self.layers.push(layer);
        self.sources.push(entity);
        let opened = SourceId(self.sources.len() - 1);
        self.descend(opened)
    }

    fn descend(mut self, source: SourceId) -> Walk<'r> {
        let bytes = &self.sources[source.0];
        let step = match MessageParser::default().parse(bytes.as_slice()) {
            Some(message) => walk_source(
                self.recognisers,
                &message,
                source,
                &mut self.layers,
                &mut self.malformed,
            ),
            None => Step::Reached(
                PartRef::new(source, &[], 0..bytes.len()),
                Findings::default(),
            ),
        };
        match step {
            Step::Reached(payload, findings) => {
                Walk::Complete(self.finish(Payload::Reached(payload), findings))
            }
            Step::TooDeep => {
                let payload = Payload::Sealed(SealReason::TooDeep);
                Walk::Complete(self.finish(payload, Findings::default()))
            }
            Step::Sealed(layer, object) => match layer.kind {
                LayerKind::Signed(_) => Walk::EmbeddedSignature(EmbeddedSignatureLayer {
                    state: self,
                    layer,
                    object,
                }),
                LayerKind::Encrypted | LayerKind::SignedAndEncrypted => {
                    Walk::Encrypted(EncryptedLayer {
                        state: self,
                        layer,
                        object,
                    })
                }
            },
        }
    }

    fn finish(mut self, payload: Payload, findings: Findings) -> Walked {
        self.malformed.extend(findings.malformed);
        Walked::new(
            self.sources,
            Envelope {
                layers: self.layers,
                payload,
                errant: findings.errant,
                inline: findings.inline,
                malformed: self.malformed,
            },
        )
    }
}

/// Walks one source from its root until the payload, a sealed layer, or the limit.
///
/// A detached layer's protected part is in the same source, so the walk goes
/// straight on into it; a sealed layer ends this source and is returned for the
/// caller to open.
fn walk_source(
    recognisers: &[&dyn LayerRecogniser],
    message: &Message<'_>,
    source: SourceId,
    layers: &mut Vec<Layer>,
    malformed: &mut Vec<MalformedLayer>,
) -> Step {
    let Some(mut part) = PartView::root(message) else {
        let whole = PartRef::new(source, &[], 0..message.raw_message.len());
        return Step::Reached(whole, Findings::default());
    };
    let mut path = Vec::new();
    loop {
        let here = PartRef::new(source, &path, part.range());
        let reached = |part, path: &[usize]| {
            Step::Reached(here.clone(), scan(recognisers, source, part, path))
        };
        let (mechanism, shape) = match recognise(recognisers, &part) {
            None => return reached(part, &path),
            Some((mechanism, Recognition::Malformed)) => {
                malformed.push(MalformedLayer {
                    mechanism,
                    part: here.clone(),
                });
                return reached(part, &path);
            }
            Some((mechanism, Recognition::Layer(shape))) => (mechanism, shape),
        };
        let mut layer = Layer::new(mechanism, shape.kind(), here.clone());
        match check(part, shape) {
            Checked::Malformed => {
                malformed.push(MalformedLayer {
                    mechanism,
                    part: here.clone(),
                });
                return reached(part, &path);
            }
            _ if layers.len() == MAX_LAYERS => return Step::TooDeep,
            Checked::Sealed { object } => return Step::Sealed(layer, object.contents().to_vec()),
            Checked::Detached {
                protected,
                signature,
            } => {
                layer.detached = Some(Detached {
                    signed: protected.range(),
                    signature: signature.contents().to_vec(),
                });
                layers.push(layer);
                part = protected;
                path.push(0);
            }
        }
    }
}

/// A recognised shape, checked against the part it was recognised on.
pub(crate) enum Checked<'a> {
    Detached {
        protected: PartView<'a>,
        signature: PartView<'a>,
    },
    Sealed {
        object: PartView<'a>,
    },
    Malformed,
}

/// Checks the structure a shape promises: `multipart/signed` has exactly two
/// children (RFC 1847 §2.1), and a sealed layer's object exists.
pub(crate) fn check(part: PartView<'_>, shape: LayerShape) -> Checked<'_> {
    match shape {
        LayerShape::DetachedSignature => {
            let mut children = part.children();
            match (children.next(), children.next(), children.next()) {
                (Some(protected), Some(signature), None) => Checked::Detached {
                    protected,
                    signature,
                },
                _ => Checked::Malformed,
            }
        }
        LayerShape::EmbeddedSignature(object) | LayerShape::Encryption(object) => {
            let object = match object {
                Object::Body => Some(part),
                Object::Child(index) => part.child(index),
            };
            object.map_or(Checked::Malformed, |object| Checked::Sealed { object })
        }
    }
}

fn redacted(f: &mut fmt::Formatter<'_>, name: &str, layer: &Layer, object: &[u8]) -> fmt::Result {
    f.debug_struct(name)
        .field("mechanism", &layer.mechanism)
        .field("kind", &layer.kind)
        .field("object_len", &object.len())
        .finish_non_exhaustive()
}

impl fmt::Debug for EncryptedLayer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        redacted(f, "EncryptedLayer", &self.layer, &self.object)
    }
}

impl fmt::Debug for EmbeddedSignatureLayer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        redacted(f, "EmbeddedSignatureLayer", &self.layer, &self.object)
    }
}
