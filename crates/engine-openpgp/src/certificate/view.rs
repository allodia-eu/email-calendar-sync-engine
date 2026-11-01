//! A certificate evaluated at one instant.

use engine_core::{
    e2e::{
        CertificateFacts, CertificateId, CertificateProblem, CertificateStatus, SenderBinding,
        Warning,
    },
    time::UtcDateTime,
};
use pgp::{
    composed::SignedPublicKey,
    packet::SignatureType,
    types::{KeyDetails, KeyVersion, SignedUser, Tag},
};

use super::{
    keys::{self, ComponentKey, EncryptionPurpose},
    selfsig::{self, Counted},
};
use crate::{
    address::{canonical_address, user_id_address},
    policy::{self, Acceptance},
    time::instant,
};

const CERTIFICATIONS: &[SignatureType] = &[
    SignatureType::CertGeneric,
    SignatureType::CertPersona,
    SignatureType::CertCasual,
    SignatureType::CertPositive,
];

/// What a certificate says at one instant: whether it is usable, which addresses
/// it binds, and which of its keys may do what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateView {
    id: CertificateId,
    created: UtcDateTime,
    status: CertificateStatus,
    expires: Option<UtcDateTime>,
    addresses: Vec<String>,
    keys: Vec<ComponentKey>,
    warnings: Vec<Warning>,
}

/// A User ID whose most recent certification counts and is not revoked.
struct BoundUserId<'c> {
    address: Option<String>,
    certification: Counted<'c>,
}

impl CertificateView {
    pub(super) fn evaluate(key: &SignedPublicKey, id: &CertificateId, time: UtcDateTime) -> Self {
        let now = time.unix_seconds();
        let primary = &key.primary_key;
        let created = primary.created_at().as_secs();
        let user_ids = bound_user_ids(key, created, now);
        let direct = selfsig::most_recent(
            &key.details.direct_signatures,
            &[SignatureType::Key],
            created,
            now,
            |signature| signature.verify_key(primary).is_ok(),
        );
        // A v6 key speaks through its Direct Key signature, which it cannot do
        // without; a v4 key through its primary User ID, by convention, falling back
        // to a Direct Key signature (RFC 9580 §5.2.3.10).
        let binding = match primary.version() {
            KeyVersion::V6 => direct,
            _ => primary_user_id(&user_ids)
                .map(|user| user.certification.clone())
                .or(direct),
        };
        let revocation = selfsig::revocation(
            &key.details.revocation_signatures,
            SignatureType::KeyRevocation,
            created,
            now,
            |signature| signature.verify_key(primary).is_ok(),
        );
        let strength = policy::public_key(primary);
        let expires = binding.as_ref().and_then(|b| keys::expiry(created, b));
        let status = if strength == Acceptance::Refuse {
            CertificateStatus::Unusable(CertificateProblem::RejectedAlgorithm)
        } else if i64::from(created) > now {
            CertificateStatus::NotYetValid
        } else if let Some(revocation) = revocation {
            CertificateStatus::Revoked(revocation)
        } else if binding.is_none() {
            CertificateStatus::Unusable(CertificateProblem::NoValidSelfSignature)
        } else if expires.is_some_and(|at| at.unix_seconds() <= now) {
            CertificateStatus::Expired
        } else {
            CertificateStatus::Usable
        };

        let mut warnings = strength.warnings().unwrap_or_default();
        let mut component_keys = Vec::new();
        if let (CertificateStatus::Usable, Some(binding)) = (status, &binding) {
            warnings.extend(binding.warnings.iter().cloned());
            component_keys.push(keys::primary(
                primary,
                binding.signature,
                expires,
                warnings.clone(),
            ));
            component_keys.extend(
                key.public_subkeys
                    .iter()
                    .filter_map(|subkey| keys::subkey(primary, subkey, now)),
            );
        }
        for component in &component_keys {
            warnings.extend(component.warnings.iter().cloned());
        }
        let mut addresses: Vec<String> = Vec::new();
        for address in user_ids.into_iter().filter_map(|user| user.address) {
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        let mut unique_warnings: Vec<Warning> = Vec::new();
        for warning in warnings {
            if !unique_warnings.contains(&warning) {
                unique_warnings.push(warning);
            }
        }
        Self {
            id: id.clone(),
            created: instant(created),
            status,
            expires,
            addresses,
            keys: component_keys,
            warnings: unique_warnings,
        }
    }

    /// Returns whether the certificate can be used.
    pub fn status(&self) -> CertificateStatus {
        self.status
    }

    /// Says whether the certificate binds `address`, compared after canonicalisation
    /// (`openpgp.md` → Addresses).
    pub fn binds(&self, address: &str) -> SenderBinding {
        if self.addresses.is_empty() {
            SenderBinding::NoAddress
        } else if canonical_address(address).is_some_and(|a| self.addresses.contains(&a)) {
            SenderBinding::Matches
        } else {
            SenderBinding::Mismatch
        }
    }

    /// Returns the keys that may make signatures; none unless the certificate is
    /// usable.
    pub fn signing_keys(&self) -> Vec<ComponentKey> {
        self.keys
            .iter()
            .filter(|key| key.signs())
            .cloned()
            .collect()
    }

    /// Returns the keys that may be encrypted to for `purpose`; none unless the
    /// certificate is usable.
    pub fn encryption_keys(&self, purpose: EncryptionPurpose) -> Vec<ComponentKey> {
        self.keys
            .iter()
            .filter(|key| key.encrypts_for(purpose))
            .cloned()
            .collect()
    }

    /// Returns the certificate's statements in mechanism-neutral terms.
    pub fn facts(&self) -> CertificateFacts {
        CertificateFacts {
            id: self.id.clone(),
            created: self.created,
            status: self.status,
            expires: self.expires,
            addresses: self.addresses.clone(),
            can_sign: self.keys.iter().any(ComponentKey::signs),
            can_encrypt: self
                .keys
                .iter()
                .any(|key| key.encrypts_for(EncryptionPurpose::ToOthers)),
            warnings: self.warnings.clone(),
        }
    }
}

/// Every User ID whose most recent certification counts at `now` and has not been
/// revoked since. A revocation is answered by a newer certification, whatever its
/// reason: the latest statement about a User ID is the one that holds.
fn bound_user_ids(key: &SignedPublicKey, created: u32, now: i64) -> Vec<BoundUserId<'_>> {
    let primary = &key.primary_key;
    key.details
        .users
        .iter()
        .filter_map(|user: &SignedUser| {
            let verify = |signature: &pgp::packet::Signature| {
                signature
                    .verify_certification(primary, Tag::UserId, &user.id)
                    .is_ok()
            };
            let certification =
                selfsig::most_recent(&user.signatures, CERTIFICATIONS, created, now, verify)?;
            let revoked = selfsig::most_recent(
                &user.signatures,
                &[SignatureType::CertRevocation],
                created,
                now,
                verify,
            )
            .is_some_and(|revocation| revocation.created >= certification.created);
            (!revoked).then(|| BoundUserId {
                address: user.id.as_str().and_then(user_id_address),
                certification,
            })
        })
        .collect()
}

/// The primary User ID: among those whose certification says so, the most recent;
/// with none flagged, the most recent overall (RFC 9580 §5.2.3.27).
fn primary_user_id<'a, 'c>(user_ids: &'a [BoundUserId<'c>]) -> Option<&'a BoundUserId<'c>> {
    user_ids.iter().max_by_key(|user| {
        (
            user.certification.signature.is_primary(),
            user.certification.created,
        )
    })
}
