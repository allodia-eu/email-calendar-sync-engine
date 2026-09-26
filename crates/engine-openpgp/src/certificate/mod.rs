//! OpenPGP certificates, and what they say at a given moment.
//!
//! rPGP parses a transferable public key and verifies one signature at a time. This
//! module decides which signatures count: the most recent valid self-signature at
//! the moment asked about, revocations by their reach, expiry from the right
//! signature, subkeys by their bindings and back-signatures (`openpgp.md`).

mod keys;
mod selfsig;
mod view;

use engine_core::{e2e::CertificateId, time::UtcDateTime};
pub use keys::{ComponentKey, EncryptionPurpose};
use pgp::{
    composed::{Deserializable, SignedPublicKey},
    ser::Serialize,
    types::{KeyDetails, KeyVersion},
};
pub use view::CertificateView;

/// Why bytes are not a certificate this engine can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CertificateError {
    /// The bytes are not an OpenPGP transferable public key.
    #[error("not an OpenPGP certificate")]
    Malformed,
    /// The bytes hold more than one certificate.
    #[error("more than one certificate")]
    NotOne,
    /// The certificate is a version this engine does not read (version 3 and older,
    /// or one it does not know).
    #[error("unsupported certificate version")]
    Unsupported,
}

/// One OpenPGP certificate (a transferable public key, RFC 9580 §10.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    key: SignedPublicKey,
    id: CertificateId,
}

impl Certificate {
    /// Parses exactly one certificate, ASCII-armoured or binary.
    ///
    /// # Errors
    ///
    /// Returns [`CertificateError::Malformed`] for bytes that are not a certificate,
    /// [`CertificateError::NotOne`] for more than one, and
    /// [`CertificateError::Unsupported`] for a version this engine does not read.
    pub fn parse(bytes: &[u8]) -> Result<Self, CertificateError> {
        let mut keys = if is_armoured(bytes) {
            if armour_blocks(bytes) > 1 {
                return Err(CertificateError::NotOne);
            }
            SignedPublicKey::from_armor_many(bytes)
                .map_err(|_| CertificateError::Malformed)?
                .0
        } else {
            SignedPublicKey::from_bytes_many(bytes).map_err(|_| CertificateError::Malformed)?
        };
        let key = keys
            .next()
            .ok_or(CertificateError::Malformed)?
            .map_err(|_| CertificateError::Malformed)?;
        if keys.next().is_some() {
            return Err(CertificateError::NotOne);
        }
        if !matches!(key.primary_key.version(), KeyVersion::V4 | KeyVersion::V6) {
            return Err(CertificateError::Unsupported);
        }
        let id = CertificateId::open_pgp(key.primary_key.fingerprint().as_bytes())
            .map_err(|_| CertificateError::Unsupported)?;
        Ok(Self { key, id })
    }

    /// Returns the certificate's identity: its primary key's fingerprint.
    pub fn id(&self) -> &CertificateId {
        &self.id
    }

    /// Returns the certificate as binary packets.
    pub fn to_bytes(&self) -> Vec<u8> {
        // Serialising a parsed certificate into memory has no failure left to meet.
        self.key.to_bytes().unwrap_or_default()
    }

    /// Evaluates the certificate at `time`.
    pub fn at(&self, time: UtcDateTime) -> CertificateView {
        CertificateView::evaluate(&self.key, &self.id, time)
    }
}

fn is_armoured(bytes: &[u8]) -> bool {
    bytes.trim_ascii_start().starts_with(b"-----BEGIN PGP ")
}

/// How many armour blocks start a line in `bytes`.
fn armour_blocks(bytes: &[u8]) -> usize {
    bytes
        .split(|&byte| byte == b'\n')
        .filter(|line| line.trim_ascii_start().starts_with(b"-----BEGIN PGP "))
        .count()
}
