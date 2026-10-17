//! `engine-openpgp` — OpenPGP (RFC 9580) framed as PGP/MIME (RFC 3156), as one
//! mechanism behind the neutral `engine-e2e` layer.
//!
//! The crate is pure: no I/O, no async, no store. Secret keys are handed in per call
//! and never kept. rPGP (the `pgp` crate) parses packets and does the cryptography;
//! what rPGP leaves open, this crate decides, and `docs/agent-guidance/openpgp.md`
//! records each decision.
//!
//! - [`PgpMime`] tells the walk which parts are PGP/MIME layers and which text carries OpenPGP
//!   inline.
//! - [`Certificate`] reads a certificate and says, at a given instant, whether it is usable, which
//!   addresses it binds, and which of its keys may sign or be encrypted to. rPGP verifies one
//!   signature at a time; which signatures *count* is decided here.
//!
//! Every normative statement in RFC 9580 and the RFC 3156 statements about OpenPGP
//! are rows in `tests/conformance/`, each covered by named tests or openly planned.

mod address;
mod certificate;
mod policy;
mod recognise;
mod time;

pub use address::canonical_address;
pub use certificate::{
    Certificate, CertificateError, CertificateView, ComponentKey, EncryptionPurpose,
};
pub use recognise::PgpMime;
