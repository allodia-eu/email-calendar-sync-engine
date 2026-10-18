//! The decisions RFC 9580 leaves to the implementation (`openpgp.md` → Policy).

use engine_core::e2e::{AlgorithmName, Warning};
use pgp::{
    crypto::{hash::HashAlgorithm, public_key::PublicKeyAlgorithm},
    ser::Serialize,
    types::{KeyDetails, PublicParams},
};

/// 2023-02-01T00:00:00Z: a SHA-1 or RIPEMD-160 self-signature made before this
/// still counts (with a warning); one made after does not.
pub(crate) const WEAK_SELF_SIGNATURE_CUT_OFF: u32 = 1_675_209_600;

/// RSA keys below this many bits are refused outright (RFC 9580 §12.4 MUST NOT).
const RSA_REFUSED_BELOW: u16 = 2048;

/// RSA keys below this many bits work, with a warning (a recorded deviation from
/// RFC 9580 §12.4's SHOULD NOT).
const RSA_WEAK_BELOW: u16 = 3072;

/// Whether something may be relied on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Acceptance {
    Accept,
    AcceptWithWarning(Warning),
    Refuse,
}

impl Acceptance {
    /// The warning to carry when accepted, or `None` when refused.
    pub(crate) fn warnings(self) -> Option<Vec<Warning>> {
        match self {
            Self::Accept => Some(Vec::new()),
            Self::AcceptWithWarning(warning) => Some(vec![warning]),
            Self::Refuse => None,
        }
    }
}

/// Whether a self-signature on `hash`, made at `created` (Unix seconds), counts.
///
/// Only the key's owner signs a self-signature, so a collision attack gains an
/// attacker nothing there; what is left is second-preimage resistance, which SHA-1
/// still has. Hence the cut-off, not an outright refusal (`openpgp.md`).
pub(crate) fn self_signature_hash(hash: HashAlgorithm, created: u32) -> Acceptance {
    match hash {
        HashAlgorithm::Sha256
        | HashAlgorithm::Sha384
        | HashAlgorithm::Sha512
        | HashAlgorithm::Sha224
        | HashAlgorithm::Sha3_256
        | HashAlgorithm::Sha3_512 => Acceptance::Accept,
        HashAlgorithm::Sha1 | HashAlgorithm::Ripemd160 if created < WEAK_SELF_SIGNATURE_CUT_OFF => {
            Acceptance::AcceptWithWarning(Warning::DeprecatedHash {
                hash: AlgorithmName::new(hash.to_string()),
            })
        }
        _ => Acceptance::Refuse,
    }
}

/// Whether a key may be used at all, by its algorithm and size.
///
/// DSA never signs or verifies (RFC 9580 §12.5), Elgamal is never encrypted to
/// (§12.6), and the deprecated single-purpose RSA and Elgamal-sign identifiers are
/// never used. Decrypting with an Elgamal key the reader holds is a separate
/// question, answered where decryption happens.
pub(crate) fn public_key(key: &impl KeyDetails) -> Acceptance {
    match key.public_params() {
        PublicParams::RSA(params) if key.algorithm() == PublicKeyAlgorithm::RSA => {
            match rsa_bits(params) {
                Some(bits) if bits >= RSA_WEAK_BELOW => Acceptance::Accept,
                Some(bits) if bits >= RSA_REFUSED_BELOW => {
                    Acceptance::AcceptWithWarning(Warning::WeakPublicKey {
                        algorithm: AlgorithmName::new("RSA"),
                        bits: u32::from(bits),
                    })
                }
                _ => Acceptance::Refuse,
            }
        }
        PublicParams::ECDSA(_)
        | PublicParams::ECDH(_)
        | PublicParams::EdDSALegacy(_)
        | PublicParams::Ed25519(_)
        | PublicParams::X25519(_)
        | PublicParams::X448(_)
        | PublicParams::Ed448(_) => Acceptance::Accept,
        _ => Acceptance::Refuse,
    }
}

/// The modulus length: the bit count heading the first MPI of the serialised
/// parameters (RFC 9580 §3.2), which avoids depending on the RSA crate's API.
fn rsa_bits(params: &impl Serialize) -> Option<u16> {
    let bytes = params.to_bytes().ok()?;
    Some(u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_hashes_always_count_for_self_signatures() {
        for hash in [
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha512,
            HashAlgorithm::Sha3_256,
        ] {
            assert_eq!(self_signature_hash(hash, u32::MAX), Acceptance::Accept);
        }
    }

    #[test]
    fn weak_hashes_count_for_self_signatures_only_before_the_cut_off() {
        for hash in [HashAlgorithm::Sha1, HashAlgorithm::Ripemd160] {
            let before = self_signature_hash(hash, WEAK_SELF_SIGNATURE_CUT_OFF - 1);
            assert!(
                matches!(before, Acceptance::AcceptWithWarning(_)),
                "{hash:?}"
            );
            assert_eq!(
                self_signature_hash(hash, WEAK_SELF_SIGNATURE_CUT_OFF),
                Acceptance::Refuse
            );
        }
    }

    #[test]
    fn md5_and_unknown_hashes_never_count() {
        for hash in [
            HashAlgorithm::Md5,
            HashAlgorithm::None,
            HashAlgorithm::Other(200),
        ] {
            assert_eq!(self_signature_hash(hash, 0), Acceptance::Refuse);
        }
    }

    #[test]
    fn a_refusal_carries_no_warnings_and_an_acceptance_its_own() {
        assert_eq!(Acceptance::Refuse.warnings(), None);
        assert_eq!(Acceptance::Accept.warnings(), Some(Vec::new()));
        let warning = Warning::DeprecatedHash {
            hash: AlgorithmName::new("SHA1"),
        };
        assert_eq!(
            Acceptance::AcceptWithWarning(warning.clone()).warnings(),
            Some(vec![warning])
        );
    }
}
