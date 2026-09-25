//! What a certificate says about itself, at one moment.

use serde::{Deserialize, Serialize};

use super::{CertificateId, Warning};
use crate::time::UtcDateTime;

/// A certificate's own statements, evaluated at one instant, in terms that do not
/// depend on its mechanism.
///
/// These are facts, not trust: a certificate can be [`Usable`](CertificateStatus::Usable)
/// and still belong to someone other than who it names. Whether to rely on it for an
/// address is the host's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateFacts {
    /// Which certificate.
    pub id: CertificateId,
    /// When its primary key was made.
    pub created: UtcDateTime,
    /// Whether it can be used at the instant it was evaluated.
    pub status: CertificateStatus,
    /// When it stops being usable by expiry; `None` when it states no expiry, or
    /// states nothing valid at all.
    pub expires: Option<UtcDateTime>,
    /// The addresses it binds, canonicalised, in the order it lists them.
    pub addresses: Vec<String>,
    /// Whether it has a key that can sign.
    pub can_sign: bool,
    /// Whether it has a key others can encrypt to.
    pub can_encrypt: bool,
    /// Weaknesses it is usable despite.
    pub warnings: Vec<Warning>,
}

/// Whether a certificate can be used at a given instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CertificateStatus {
    /// It can be used.
    Usable,
    /// Its key was made after the instant asked about.
    NotYetValid,
    /// It had expired by the instant asked about.
    Expired,
    /// Its owner revoked it.
    Revoked(Revocation),
    /// It cannot be used at any time.
    Unusable(CertificateProblem),
}

/// A revocation, and how far back it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Revocation {
    /// When the revocation was made.
    pub at: UtcDateTime,
    /// Which signatures it takes back.
    pub reach: RevocationReach,
}

/// Which signatures a revocation takes back.
///
/// OpenPGP distinguishes a key that was *compromised*, whose every signature is
/// suspect, from one that was *superseded* or *retired*, whose earlier signatures
/// stand (RFC 9580 §5.2.3.31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RevocationReach {
    /// Every signature the key ever made: it may have been compromised.
    Retroactive,
    /// Signatures made from the revocation on; earlier ones stand.
    FromThen,
}

/// Why a certificate cannot be used at any time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CertificateProblem {
    /// No self-signature that verifies says what the key may do.
    NoValidSelfSignature,
    /// Its primary key uses an algorithm, or a size, the engine refuses.
    RejectedAlgorithm,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_round_trip_through_json() {
        let facts = CertificateFacts {
            id: CertificateId::open_pgp(&[4; 20]).expect("v4"),
            created: "2025-01-01T00:00:00Z".parse().expect("time"),
            status: CertificateStatus::Revoked(Revocation {
                at: "2025-06-01T00:00:00Z".parse().expect("time"),
                reach: RevocationReach::FromThen,
            }),
            expires: None,
            addresses: vec!["alice@example.org".to_owned()],
            can_sign: true,
            can_encrypt: false,
            warnings: Vec::new(),
        };
        let json = serde_json::to_string(&facts).expect("serialize");
        let back: CertificateFacts = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, facts);
    }
}
