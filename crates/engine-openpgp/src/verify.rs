//! Checking message signatures against certificates, into neutral verdicts.
//!
//! rPGP says whether the mathematics holds. Everything else a verdict carries is
//! decided here: whether the signature is well formed (RFC 9580 §5.2.5), which
//! certificate made it, whether that certificate could sign at the moment it did,
//! whether the hash is acceptable, whether it was meant for this reader
//! (§5.2.3.36), and whether it names the sender.

use engine_core::{
    e2e::{
        CertificateId, CertificateProblem, CertificateStatus, ChainStatus, GoodSignature,
        IssuerHint, Mechanism, SignatureVerdict, UnusableReason,
    },
    time::UtcDateTime,
};
use pgp::{
    composed::{Deserializable, DetachedSignature},
    crypto::public_key::PublicKeyAlgorithm,
    packet::{Signature, SignatureType, SignatureVersion, SubpacketData, SubpacketType},
    types::KeyVersion,
};

use crate::{
    Certificate,
    certificate::KeyMaterial,
    policy::{self, Acceptance},
    time::instant,
};

/// Where a signature was found, which decides whether an Intended Recipient
/// Fingerprint admits it (RFC 9580 §5.2.3.36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureContext<'a> {
    /// In readable content, outside any encryption.
    Cleartext,
    /// Inside encryption this reader opened with the certificate named.
    DecryptedWith(&'a CertificateId),
}

/// Checks signatures for one message.
#[derive(Debug, Clone, Copy)]
pub struct Verifier<'a> {
    certificates: &'a [Certificate],
    sender: &'a str,
    now: UtcDateTime,
    context: SignatureContext<'a>,
}

impl<'a> Verifier<'a> {
    /// A verifier for a message from `sender` (its `From` address), checked at `now`
    /// against `certificates`, in cleartext.
    pub fn new(certificates: &'a [Certificate], sender: &'a str, now: UtcDateTime) -> Self {
        Self {
            certificates,
            sender,
            now,
            context: SignatureContext::Cleartext,
        }
    }

    /// The same verifier, for signatures found inside `context`.
    #[must_use]
    pub fn within(self, context: SignatureContext<'a>) -> Self {
        Self { context, ..self }
    }

    /// Checks a detached signature part (armoured or binary; it may hold several
    /// signatures) over the exact `signed` bytes, one verdict per signature.
    ///
    /// A part with no signature that parses yields a single
    /// [`Malformed`](SignatureVerdict::Malformed) verdict: something claimed to be a
    /// signature, and nothing that was one can be said to be good.
    pub fn detached(&self, signed: &[u8], part: &[u8]) -> Vec<SignatureVerdict> {
        let signatures = parse_signatures(part);
        if signatures.is_empty() {
            return vec![SignatureVerdict::Malformed];
        }
        signatures
            .iter()
            .map(|signature| {
                self.verdict(signature, |key: KeyMaterial<'_>| {
                    key.verifies(signature, signed)
                })
            })
            .collect()
    }

    /// The verdict on one signature, with `verifies` doing the mathematics against
    /// the issuer's key.
    pub(crate) fn verdict(
        &self,
        signature: &Signature,
        verifies: impl Fn(KeyMaterial<'_>) -> bool,
    ) -> SignatureVerdict {
        if !matches!(
            signature.version(),
            SignatureVersion::V4 | SignatureVersion::V6
        ) {
            return SignatureVerdict::Unsupported;
        }
        let Some(created) = well_formed(signature) else {
            return SignatureVerdict::Malformed;
        };
        let Some((certificate, key)) = self
            .certificates
            .iter()
            .find_map(|certificate| Some((certificate, certificate.issuer(signature)?)))
        else {
            return SignatureVerdict::UnknownSigner {
                issuer: issuer_hint(signature),
            };
        };
        let signer = certificate.id().clone();
        if weak_hash_for_key(signature, key) {
            return SignatureVerdict::Malformed;
        }
        if !verifies(key) {
            return SignatureVerdict::Bad { signer };
        }
        let unusable = |reason| SignatureVerdict::Unusable {
            signer: signer.clone(),
            reason,
        };
        let hash = signature
            .hash_alg()
            .map_or(Acceptance::Refuse, policy::message_signature_hash);
        if hash == Acceptance::Refuse {
            return unusable(UnusableReason::RejectedAlgorithm);
        }
        let when = instant(created);
        let view = certificate.at(when);
        match view.status() {
            CertificateStatus::Usable => {}
            CertificateStatus::NotYetValid => return SignatureVerdict::Malformed,
            CertificateStatus::Expired => return unusable(UnusableReason::Expired),
            CertificateStatus::Revoked(_) => return unusable(UnusableReason::Revoked),
            CertificateStatus::Unusable(CertificateProblem::RejectedAlgorithm) => {
                return unusable(UnusableReason::RejectedAlgorithm);
            }
            CertificateStatus::Unusable(CertificateProblem::NoValidSelfSignature) => {
                return unusable(UnusableReason::NotSigningCapable);
            }
        }
        let fingerprint = key.fingerprint();
        let Some(signing_key) = view
            .signing_keys()
            .into_iter()
            .find(|candidate| candidate.fingerprint() == fingerprint.as_slice())
        else {
            return unusable(UnusableReason::NotSigningCapable);
        };
        if expired_by(signature, created, self.now) {
            return unusable(UnusableReason::Expired);
        }
        if !self.meant_for_this_reader(signature) {
            return SignatureVerdict::NotForThisRecipient { signer };
        }
        let mut warnings = view.facts().warnings;
        for warning in signing_key.warnings() {
            if !warnings.contains(warning) {
                warnings.push(warning.clone());
            }
        }
        SignatureVerdict::Good(GoodSignature {
            signer,
            created: when,
            sender: view.binds(self.sender),
            chain: ChainStatus::NotApplicable,
            warnings,
        })
    }

    /// An Intended Recipient Fingerprint admits a signature only inside encryption
    /// opened with one of the certificates it names.
    fn meant_for_this_reader(&self, signature: &Signature) -> bool {
        let intended: Vec<&[u8]> = signature
            .config()
            .into_iter()
            .flat_map(pgp::packet::SignatureConfig::hashed_subpackets)
            .filter_map(|subpacket| match &subpacket.data {
                SubpacketData::IntendedRecipientFingerprint(fingerprint) => {
                    Some(fingerprint.as_bytes())
                }
                _ => None,
            })
            .collect();
        match self.context {
            _ if intended.is_empty() => true,
            SignatureContext::Cleartext => false,
            SignatureContext::DecryptedWith(reader) => intended.contains(&reader.fingerprint()),
        }
    }
}

/// Every signature packet in a signature part, armoured or binary.
fn parse_signatures(part: &[u8]) -> Vec<Signature> {
    let parsed = if part.trim_ascii_start().starts_with(b"-----BEGIN PGP ") {
        DetachedSignature::from_armor_many(part).map(|(signatures, _)| signatures)
    } else {
        DetachedSignature::from_bytes_many(part)
    };
    parsed
        .map(|signatures| {
            signatures
                .filter_map(Result::ok)
                .map(|d| d.signature)
                .collect()
        })
        .unwrap_or_default()
}

/// The signature's creation time, when it is well formed as a data signature:
/// a binary or text document signature, stating when it was made, with no
/// critical subpacket this engine does not know (§5.2.3.7) and an Issuer
/// Fingerprint of its own version (§5.2.3.35).
fn well_formed(signature: &Signature) -> Option<u32> {
    let config = signature.config()?;
    if !matches!(
        signature.typ()?,
        SignatureType::Binary | SignatureType::Text
    ) {
        return None;
    }
    for subpacket in config.hashed_subpackets() {
        if subpacket.is_critical && matches!(subpacket.typ(), SubpacketType::Other(_)) {
            return None;
        }
        if let SubpacketData::IssuerFingerprint(fingerprint) = &subpacket.data {
            let aligned = matches!(
                (signature.version(), fingerprint.version()),
                (SignatureVersion::V4, Some(KeyVersion::V4))
                    | (SignatureVersion::V6, Some(KeyVersion::V6))
            );
            if !aligned {
                return None;
            }
        }
    }
    Some(signature.created()?.as_secs())
}

/// An EdDSA signature on a hash shorter than its curve demands is malformed
/// (RFC 9580 §5.2.3.3 to §5.2.3.5), not merely bad.
fn weak_hash_for_key(signature: &Signature, key: KeyMaterial<'_>) -> bool {
    let bits = signature
        .hash_alg()
        .and_then(pgp::crypto::hash::HashAlgorithm::digest_size)
        .map_or(0, |bytes| bytes * 8);
    match key.algorithm() {
        PublicKeyAlgorithm::Ed25519 | PublicKeyAlgorithm::EdDSALegacy => bits < 256,
        PublicKeyAlgorithm::Ed448 => bits < 512,
        _ => false,
    }
}

/// Whether the signature's own Signature Expiration Time has passed by `now`.
fn expired_by(signature: &Signature, created: u32, now: UtcDateTime) -> bool {
    signature
        .signature_expiration_time()
        .map(pgp::types::Duration::as_secs)
        .is_some_and(|lifetime| {
            lifetime != 0 && i64::from(created) + i64::from(lifetime) <= now.unix_seconds()
        })
}

/// What the signature says about its issuer, for a signer nobody has a certificate
/// for: its fingerprint when it names one, else its key ID.
fn issuer_hint(signature: &Signature) -> Option<IssuerHint> {
    let octets: Box<[u8]> = match signature.issuer_fingerprint().first() {
        Some(fingerprint) => fingerprint.as_bytes().into(),
        None => signature.issuer_key_id().first()?.as_ref().into(),
    };
    Some(IssuerHint::Handle {
        mechanism: Mechanism::OpenPgp,
        octets,
    })
}
