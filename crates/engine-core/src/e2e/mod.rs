//! End-to-end message protection: the neutral facts a host sees.
//!
//! RFC 9787 describes signed and encrypted mail without assuming a mechanism: a
//! message has a *cryptographic envelope* of *layers* around a *payload*, and one
//! *cryptographic summary* (§3.1, §4). S/MIME and PGP/MIME are two ways to spell
//! those layers (§4.1.1, §4.1.2). This module holds the vocabulary both spellings
//! share, so a host handles a verdict the same way whichever mechanism produced it:
//!
//! - [`Mechanism`] and [`CertificateId`] name *who* protected a message and *with which
//!   certificate*, without mixing an OpenPGP fingerprint with anything else.
//! - [`LayerKind`] says what one layer does.
//! - [`SignatureVerdict`] reports what checking one signature found, as facts: whether a signer is
//!   *trusted* is the host's decision, never the engine's.
//! - [`DecryptionOutcome`] reports what opening an encrypted layer found, including whether the
//!   content was integrity-protected ([`Integrity`]).
//! - [`Summary`] reduces an envelope to the one status RFC 9787 §3.2 asks a message to carry.
//!
//! Everything here is data plus the summary rule. Walking a MIME tree into layers
//! lives in `engine-e2e`; the cryptography lives in a mechanism crate.

mod certificate;
mod decryption;
mod summary;
mod verdict;

pub use certificate::{CertificateId, CertificateIdError, Mechanism};
pub use decryption::{
    AlgorithmName, Decryption, DecryptionFailure, DecryptionOutcome, Integrity, Warning,
};
pub use summary::{LayerKind, LayerSignatures, SignatureForm, Summary};
pub use verdict::{
    ChainStatus, GoodSignature, IssuerHint, SenderBinding, SignatureVerdict, UnusableReason,
};
