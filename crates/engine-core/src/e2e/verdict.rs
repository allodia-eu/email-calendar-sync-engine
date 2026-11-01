//! What checking one signature found.

use serde::{Deserialize, Serialize};

use super::{CertificateId, Mechanism, Warning};
use crate::time::UtcDateTime;

/// The result of checking one signature.
///
/// A verdict reports cryptographic facts. It never says whether the signer is
/// *trusted*: accepting a certificate for an address is the host's decision, and a
/// [`Good`](Self::Good) verdict from a certificate the user never accepted is still
/// only a fact about the mathematics and the certificate's own statements.
///
/// Only [`Good`](Self::Good) can make a message Verified or Confidential
/// ([`Summary`](super::Summary)); every other verdict counts as no signature at all
/// (RFC 9580 §5.2.5: a malformed or unknown signature is never good, and never
/// stops the other signatures from being checked).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureVerdict {
    /// The signature is valid, made by a usable certificate.
    Good(GoodSignature),
    /// The certificate is known and the signature does not verify: the content, or
    /// the signature, was altered.
    Bad {
        /// The certificate the signature claims.
        signer: CertificateId,
    },
    /// No certificate for the issuer is available, so nothing could be checked.
    UnknownSigner {
        /// What the signature says about its issuer, when it says anything.
        issuer: Option<IssuerHint>,
    },
    /// The signature verifies, but the certificate could not make it at that time.
    Unusable {
        /// The certificate that made the signature.
        signer: CertificateId,
        /// Why the certificate could not.
        reason: UnusableReason,
    },
    /// The signature is addressed to other recipients: it names intended recipients
    /// and this reader's certificate is not one of them (RFC 9580 §5.2.3.36). A
    /// signed message forwarded whole to a third party lands here.
    NotForThisRecipient {
        /// The certificate that made the signature.
        signer: CertificateId,
    },
    /// The signature uses a version or an algorithm this engine does not implement.
    Unsupported,
    /// The signature could not be parsed, or breaks a structural rule of its
    /// mechanism (an unknown critical subpacket, mismatched check octets).
    Malformed,
}

impl SignatureVerdict {
    /// Returns the good signature when this verdict is one.
    pub fn as_good(&self) -> Option<&GoodSignature> {
        match self {
            Self::Good(good) => Some(good),
            _ => None,
        }
    }
}

/// A valid signature, with what it says about its signer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoodSignature {
    /// The certificate that made the signature.
    pub signer: CertificateId,
    /// When the signature says it was made.
    pub created: UtcDateTime,
    /// Whether the certificate names the message's apparent sender.
    pub sender: SenderBinding,
    /// Where the certificate stands in a chain of issuers.
    pub chain: ChainStatus,
    /// Weaknesses the signature was accepted despite.
    pub warnings: Vec<Warning>,
}

/// Whether a signing certificate belongs to the address the message claims to be
/// from.
///
/// A good signature from a certificate for someone else proves only that *someone*
/// signed. RFC 9787 §3.1 counts a message as Verified only with "a valid signature
/// from the apparent sender", S/MIME requires the binding (RFC 8550 §6), and
/// OpenPGP allows checking it (RFC 9580 §5.2.3.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SenderBinding {
    /// The certificate binds the sending address.
    Matches,
    /// The certificate binds addresses, and the sending address is not among them.
    Mismatch,
    /// The certificate binds no address to compare.
    NoAddress,
}

/// Where a signing certificate stands in a chain of issuers.
///
/// OpenPGP has no issuer chain, so its signatures always report
/// [`NotApplicable`](Self::NotApplicable). S/MIME trust comes from a PKIX chain
/// (RFC 8550), and adds its states here when it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChainStatus {
    /// The mechanism has no chain of issuers.
    NotApplicable,
}

/// Why a certificate could not make a signature it verifies against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnusableReason {
    /// The certificate, or the key that signed, had expired when it signed.
    Expired,
    /// The certificate, or the key that signed, is revoked in a way that covers the
    /// signature.
    Revoked,
    /// The key that signed is not certified for signing.
    NotSigningCapable,
    /// The signature relies on an algorithm the engine's policy no longer accepts
    /// for it (a deprecated hash at the signature's date, a public key too weak).
    RejectedAlgorithm,
}

/// What a signature says about its issuer when no certificate for it is known.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IssuerHint {
    /// The issuer's full certificate identity.
    Certificate(CertificateId),
    /// A shorter handle that narrows the issuer down without identifying it, such as
    /// an OpenPGP key ID (RFC 9580 §5.2.3.12). Several certificates may share one.
    Handle {
        /// The mechanism the handle belongs to.
        mechanism: Mechanism,
        /// The handle's octets.
        octets: Box<[u8]>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> CertificateId {
        CertificateId::open_pgp(&[3; 20]).expect("v4")
    }

    fn good() -> GoodSignature {
        GoodSignature {
            signer: signer(),
            created: "2026-09-25T10:00:00Z".parse().expect("time"),
            sender: SenderBinding::Matches,
            chain: ChainStatus::NotApplicable,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn only_a_good_verdict_yields_a_good_signature() {
        assert_eq!(
            SignatureVerdict::Good(good()).as_good().map(|g| &g.signer),
            Some(&signer()),
        );
        for verdict in [
            SignatureVerdict::Bad { signer: signer() },
            SignatureVerdict::UnknownSigner { issuer: None },
            SignatureVerdict::Unusable {
                signer: signer(),
                reason: UnusableReason::Revoked,
            },
            SignatureVerdict::NotForThisRecipient { signer: signer() },
            SignatureVerdict::Unsupported,
            SignatureVerdict::Malformed,
        ] {
            assert!(verdict.as_good().is_none(), "{verdict:?}");
        }
    }

    #[test]
    fn verdicts_round_trip_through_json() {
        let verdicts = vec![
            SignatureVerdict::Good(good()),
            SignatureVerdict::UnknownSigner {
                issuer: Some(IssuerHint::Handle {
                    mechanism: Mechanism::OpenPgp,
                    octets: vec![1, 2, 3, 4, 5, 6, 7, 8].into(),
                }),
            },
        ];
        let json = serde_json::to_string(&verdicts).expect("serialize");
        let back: Vec<SignatureVerdict> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, verdicts);
    }
}
