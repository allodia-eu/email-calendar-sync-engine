//! The keys in a certificate that may be used, and for what (RFC 9580 §5.2.3.29,
//! §5.2.1.8, §10.1.5).

use engine_core::{e2e::Warning, time::UtcDateTime};
use pgp::{
    composed::SignedPublicSubKey,
    crypto::public_key::PublicKeyAlgorithm,
    packet::{KeyFlags, PublicKey, Signature, SignatureType},
    types::KeyDetails,
};

use super::selfsig::{self, Counted};
use crate::{policy, time::instant_at};

/// Who an encryption is for, which decides the key flag it needs (`openpgp.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncryptionPurpose {
    /// Someone else will read it: the key must be flagged for communications (0x04).
    ToOthers,
    /// The keyholder will read it later, such as a draft: communications (0x04) or
    /// storage (0x08) will do.
    ToSelfStorage,
}

/// One key of a certificate, usable at the instant the certificate was evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentKey {
    fingerprint: Vec<u8>,
    primary: bool,
    algorithm: PublicKeyAlgorithm,
    flags: KeyFlags,
    expires: Option<UtcDateTime>,
    pub(super) warnings: Vec<Warning>,
}

impl ComponentKey {
    /// Returns the key's own fingerprint (a subkey's differs from the certificate's).
    pub fn fingerprint(&self) -> &[u8] {
        &self.fingerprint
    }

    /// Whether this is the certificate's primary key.
    pub fn is_primary(&self) -> bool {
        self.primary
    }

    /// Returns when the key expires, if it states an expiry.
    pub fn expires(&self) -> Option<UtcDateTime> {
        self.expires
    }

    pub(super) fn signs(&self) -> bool {
        self.flags.sign()
            && matches!(
                self.algorithm,
                PublicKeyAlgorithm::RSA
                    | PublicKeyAlgorithm::ECDSA
                    | PublicKeyAlgorithm::EdDSALegacy
                    | PublicKeyAlgorithm::Ed25519
                    | PublicKeyAlgorithm::Ed448
            )
    }

    pub(super) fn encrypts_for(&self, purpose: EncryptionPurpose) -> bool {
        let flagged = match purpose {
            EncryptionPurpose::ToOthers => self.flags.encrypt_comms(),
            EncryptionPurpose::ToSelfStorage => {
                self.flags.encrypt_comms() || self.flags.encrypt_storage()
            }
        };
        flagged
            && matches!(
                self.algorithm,
                PublicKeyAlgorithm::RSA
                    | PublicKeyAlgorithm::ECDH
                    | PublicKeyAlgorithm::X25519
                    | PublicKeyAlgorithm::X448
            )
    }
}

/// The primary key as a component, with the flags and expiry its binding states.
pub(super) fn primary(
    key: &PublicKey,
    binding: &Signature,
    expires: Option<UtcDateTime>,
    warnings: Vec<Warning>,
) -> ComponentKey {
    ComponentKey {
        fingerprint: key.fingerprint().as_bytes().to_vec(),
        primary: true,
        algorithm: key.algorithm(),
        flags: binding.key_flags(),
        expires,
        warnings,
    }
}

/// A subkey that is usable at `now`, or `None`.
///
/// It must be the primary key's version, made by `now`, of an acceptable algorithm,
/// bound by a self-signature that counts (with a valid back-signature if it may
/// sign), not revoked, and not expired.
pub(super) fn subkey(
    primary: &PublicKey,
    subkey: &SignedPublicSubKey,
    now: i64,
) -> Option<ComponentKey> {
    let key = &subkey.key;
    let created = key.created_at().as_secs();
    if key.version() != primary.version() || i64::from(created) > now {
        return None;
    }
    let mut warnings = policy::public_key(key).warnings()?;
    let binding = selfsig::most_recent(
        &subkey.signatures,
        &[SignatureType::SubkeyBinding],
        created,
        now,
        |signature| {
            signature.verify_subkey_binding(primary, key).is_ok()
                && (!signature.key_flags().sign() || backed(signature, primary, subkey))
        },
    )?;
    let revoked = selfsig::revocation(
        &subkey.signatures,
        SignatureType::SubkeyRevocation,
        created,
        now,
        |signature| signature.verify_subkey_binding(primary, key).is_ok(),
    );
    if revoked.is_some() {
        return None;
    }
    let expires = expiry(created, &binding);
    if expires.is_some_and(|at| at.unix_seconds() <= now) {
        return None;
    }
    warnings.extend(binding.warnings);
    Some(ComponentKey {
        fingerprint: key.fingerprint().as_bytes().to_vec(),
        primary: false,
        algorithm: key.algorithm(),
        flags: binding.signature.key_flags(),
        expires,
        warnings,
    })
}

/// Whether a signing subkey's binding carries a valid back-signature: a 0x19
/// signature by the subkey over the primary key (RFC 9580 §5.2.1.8).
fn backed(binding: &Signature, primary: &PublicKey, subkey: &SignedPublicSubKey) -> bool {
    binding.embedded_signature().is_some_and(|back| {
        back.typ() == Some(SignatureType::KeyBinding)
            && back
                .hash_alg()
                .zip(back.created())
                .is_some_and(|(hash, created)| {
                    policy::self_signature_hash(hash, created.as_secs())
                        .warnings()
                        .is_some()
                })
            && back
                .verify_primary_key_binding(&subkey.key, primary)
                .is_ok()
    })
}

/// When a key bound by `binding` expires: Key Expiration Time counts from the key's
/// creation, and zero means never.
pub(super) fn expiry(created: u32, binding: &Counted<'_>) -> Option<UtcDateTime> {
    let lifetime = binding.signature.key_expiration_time()?.as_secs();
    if lifetime == 0 {
        return None;
    }
    Some(instant_at(i64::from(created) + i64::from(lifetime)))
}
