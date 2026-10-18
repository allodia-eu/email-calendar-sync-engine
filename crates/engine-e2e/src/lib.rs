//! `engine-e2e` — the neutral end-to-end protection layer.
//!
//! RFC 9787 describes a protected message as a *cryptographic envelope* of layers
//! around a *payload*, with any layer found inside the payload *errant* (§4). This
//! crate finds that structure in a MIME message, for every mechanism at once:
//!
//! - A mechanism registers a [`LayerRecogniser`] that says which parts are its layers. The
//!   recogniser looks at structure only; this crate holds no cryptography.
//! - [`walk`] goes from the root inwards. A detached signature's protected part is readable, so the
//!   walk goes straight on; a sealed layer (encrypted, or an embedded signature) pauses the walk as
//!   a [`Walk`] step that the caller opens and resumes. The caller brings the keys; the engine
//!   never holds them.
//! - The finished [`Walked`] carries the [`Envelope`] (layers, payload, errant layers, inline
//!   findings, malformed layers), the exact bytes each detached signature covers, and the
//!   [`Summary`](engine_core::e2e::Summary) once verdicts are in.
//!
//! # Invariants
//!
//! - **Errant layers never count.** Only the envelope feeds the summary (RFC 9787 §4.5).
//! - **Nothing sealed is summarised.** A payload not reached yields no summary, so a host cannot
//!   show a message it could not open as Confidential or otherwise.
//! - **Hostile input never panics**, and nesting stops at [`MAX_LAYERS`].
//! - **Decrypted content is not printed.** [`Walked`] and the paused steps redact their `Debug`.
//!
//! The neutral facts a host sees (mechanisms, certificates, verdicts, the summary)
//! live in [`engine_core::e2e`], so hosts and stores need not depend on this crate.

mod envelope;
mod recognise;
mod scan;
mod walk;
mod walked;

pub use envelope::{
    Envelope, ErrantLayer, InlineFinding, Layer, LayerIndex, MalformedLayer, PartRef, Payload,
    SealReason, SourceId, UnwrapFailure,
};
pub use recognise::{InlineKind, LayerRecogniser, LayerShape, Object, PartView, Recognition};
pub use walk::{EmbeddedSignatureLayer, EncryptedLayer, MAX_LAYERS, Walk, walk};
pub use walked::{DetachedSignature, Walked};
