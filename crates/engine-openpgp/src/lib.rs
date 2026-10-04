//! `engine-openpgp` — OpenPGP (RFC 9580) framed as PGP/MIME (RFC 3156), as one
//! mechanism behind the neutral `engine-e2e` layer.
//!
//! The crate is pure: no I/O, no async, no store. Secret keys are handed in per call
//! and never kept. rPGP (the `pgp` crate) parses packets and does the cryptography;
//! what rPGP leaves open, this crate decides, and `docs/agent-guidance/openpgp.md`
//! records each decision.
//!
//! So far it holds [`PgpMime`], the recogniser that tells the walk which parts are
//! PGP/MIME layers and which text carries OpenPGP inline.
//!
//! Every normative statement in RFC 9580 and the RFC 3156 statements about OpenPGP
//! are rows in `tests/conformance/`, each covered by named tests or openly planned.

mod recognise;

pub use recognise::PgpMime;
