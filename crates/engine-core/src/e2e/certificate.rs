//! Which mechanism protected a message, and which certificate did it.

use core::fmt;

use serde::{Deserialize, Serialize};

/// A way of protecting mail end to end.
///
/// RFC 9787 §4.1 defines layers for two mechanisms side by side. Only OpenPGP is
/// implemented; the enum exists so a certificate, a verdict or a layer always says
/// which one it belongs to, and S/MIME joins as a variant rather than as a second
/// set of types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Mechanism {
    /// OpenPGP (RFC 9580), framed as PGP/MIME (RFC 3156).
    OpenPgp,
}

/// Error returned when a [`CertificateId`] is built from bytes that cannot be one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CertificateIdError {
    /// The fingerprint length is not one the mechanism defines.
    #[error("a {mechanism:?} fingerprint cannot be {length} octets")]
    Length {
        /// The mechanism the fingerprint was offered for.
        mechanism: Mechanism,
        /// The number of octets supplied.
        length: usize,
    },
}

/// Identifies one certificate: a mechanism plus that mechanism's fingerprint.
///
/// For OpenPGP the fingerprint is the primary key's: 20 octets for a version 4 key
/// and 32 octets for a version 6 key (RFC 9580 §5.5.4). Version 3 keys, whose
/// 16-octet fingerprint is an MD5 hash, are not accepted (RFC 9580 §5.5.2.1
/// deprecates them). A key ID is never a `CertificateId`: eight octets name a key
/// ambiguously, and an ambiguous identity is exactly what this type exists to
/// rule out ([`IssuerHint`](super::IssuerHint) carries one where a signature
/// offers nothing better).
///
/// `Display` prints the uppercase hexadecimal fingerprint, the form OpenPGP tools
/// print. A fingerprint identifies a person, so hosts keep it out of logs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "Stored", into = "Stored")]
pub struct CertificateId {
    mechanism: Mechanism,
    fingerprint: Box<[u8]>,
}

impl CertificateId {
    /// Builds the id of an OpenPGP certificate from its primary key's fingerprint.
    ///
    /// # Errors
    ///
    /// Returns [`CertificateIdError::Length`] unless `fingerprint` is 20 octets
    /// (version 4) or 32 octets (version 6).
    pub fn open_pgp(fingerprint: &[u8]) -> Result<Self, CertificateIdError> {
        match fingerprint.len() {
            20 | 32 => Ok(Self {
                mechanism: Mechanism::OpenPgp,
                fingerprint: fingerprint.into(),
            }),
            length => Err(CertificateIdError::Length {
                mechanism: Mechanism::OpenPgp,
                length,
            }),
        }
    }

    /// Returns the mechanism this certificate belongs to.
    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }

    /// Returns the fingerprint octets.
    pub fn fingerprint(&self) -> &[u8] {
        &self.fingerprint
    }
}

/// The serialized shape, checked on the way back in so a stored id cannot skip the
/// length rule.
#[derive(Serialize, Deserialize)]
struct Stored {
    mechanism: Mechanism,
    fingerprint: Vec<u8>,
}

impl TryFrom<Stored> for CertificateId {
    type Error = CertificateIdError;

    fn try_from(stored: Stored) -> Result<Self, Self::Error> {
        match stored.mechanism {
            Mechanism::OpenPgp => Self::open_pgp(&stored.fingerprint),
        }
    }
}

impl From<CertificateId> for Stored {
    fn from(id: CertificateId) -> Self {
        Self {
            mechanism: id.mechanism,
            fingerprint: id.fingerprint.into_vec(),
        }
    }
}

impl fmt::Display for CertificateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fingerprint
            .iter()
            .try_for_each(|octet| write!(f, "{octet:02X}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_pgp_accepts_version_4_and_version_6_fingerprints() {
        let v4 = CertificateId::open_pgp(&[0xAB; 20]).expect("v4");
        let v6 = CertificateId::open_pgp(&[0x01; 32]).expect("v6");
        assert_eq!(v4.mechanism(), Mechanism::OpenPgp);
        assert_eq!(v4.fingerprint().len(), 20);
        assert_eq!(v6.fingerprint().len(), 32);
        assert_ne!(v4, v6);
    }

    #[test]
    fn open_pgp_rejects_version_3_and_key_id_lengths() {
        for length in [0, 8, 16, 21, 64] {
            assert_eq!(
                CertificateId::open_pgp(&vec![0; length]),
                Err(CertificateIdError::Length {
                    mechanism: Mechanism::OpenPgp,
                    length,
                }),
            );
        }
    }

    #[test]
    fn displays_as_uppercase_hex() {
        let mut bytes = [0u8; 20];
        bytes[0] = 0x0a;
        bytes[19] = 0xff;
        let id = CertificateId::open_pgp(&bytes).expect("v4");
        let shown = id.to_string();
        assert_eq!(shown.len(), 40);
        assert!(shown.starts_with("0A00"));
        assert!(shown.ends_with("00FF"));
    }

    #[test]
    fn round_trips_through_json() {
        let id = CertificateId::open_pgp(&[7; 32]).expect("v6");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: CertificateId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn deserializing_checks_the_length_too() {
        let json = r#"{"mechanism":"OpenPgp","fingerprint":[1,2,3]}"#;
        assert!(serde_json::from_str::<CertificateId>(json).is_err());
    }
}
