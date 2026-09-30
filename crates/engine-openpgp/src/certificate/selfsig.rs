//! Which self-signatures count, and which revocations apply (RFC 9580 §5.2.3.10,
//! §5.2.3.31).

use engine_core::e2e::{Revocation, RevocationReach, Warning};
use pgp::packet::{RevocationCode, Signature, SignatureType};

use crate::{policy, time::instant};

/// A self-signature that counts.
#[derive(Debug, Clone)]
pub(super) struct Counted<'c> {
    pub(super) signature: &'c Signature,
    pub(super) created: u32,
    pub(super) warnings: Vec<Warning>,
}

/// Whether `signature` counts at `now`: made no earlier than the component it binds
/// (`not_before`) and not after `now`, not expired, on a hash the policy accepts
/// for self-signatures, and verifying.
pub(super) fn counts_at(
    signature: &Signature,
    not_before: u32,
    now: i64,
    verify: impl Fn(&Signature) -> bool,
) -> Option<Counted<'_>> {
    let created = signature.created()?.as_secs();
    if created < not_before || i64::from(created) > now {
        return None;
    }
    if let Some(lifetime) = signature
        .signature_expiration_time()
        .map(pgp::types::Duration::as_secs)
        && lifetime != 0
        && i64::from(created) + i64::from(lifetime) <= now
    {
        return None;
    }
    let warnings = policy::self_signature_hash(signature.hash_alg()?, created).warnings()?;
    verify(signature).then_some(Counted {
        signature,
        created,
        warnings,
    })
}

/// The most recent of `signatures` of type `kind` that counts at `now`; a newer
/// one that does not count is skipped, not fatal.
pub(super) fn most_recent<'c>(
    signatures: &'c [Signature],
    kinds: &[SignatureType],
    not_before: u32,
    now: i64,
    verify: impl Fn(&Signature) -> bool,
) -> Option<Counted<'c>> {
    signatures
        .iter()
        .filter(|signature| signature.typ().is_some_and(|typ| kinds.contains(&typ)))
        .filter_map(|signature| counts_at(signature, not_before, now, &verify))
        .max_by_key(|counted| counted.created)
}

/// The revocation among `signatures` of type `kind` in force at `now`, if any.
///
/// A retroactive revocation is in force whenever it was made, even after `now`: a
/// key that may have been compromised makes every signature it ever made suspect.
/// One that takes back only later signatures is in force from its own date. When
/// both kinds are present, the retroactive one is reported.
pub(super) fn revocation(
    signatures: &[Signature],
    kind: SignatureType,
    not_before: u32,
    now: i64,
    verify: impl Fn(&Signature) -> bool,
) -> Option<Revocation> {
    signatures
        .iter()
        .filter(|signature| signature.typ() == Some(kind))
        .filter_map(|signature| {
            let created = signature.created()?.as_secs();
            let reach = reach(signature.revocation_reason_code());
            let in_force = reach == RevocationReach::Retroactive || i64::from(created) <= now;
            let sound = created >= not_before
                && policy::self_signature_hash(signature.hash_alg()?, created)
                    .warnings()
                    .is_some();
            (in_force && sound && verify(signature)).then_some((reach, created))
        })
        .min_by_key(|(reach, created)| (*reach != RevocationReach::Retroactive, *created))
        .map(|(reach, created)| Revocation {
            at: instant(created),
            reach,
        })
}

/// Superseded, retired and User ID no longer valid take back only later
/// signatures; every other reason, and none at all, reaches back (`openpgp.md`).
fn reach(reason: Option<&RevocationCode>) -> RevocationReach {
    match reason {
        Some(
            RevocationCode::KeySuperseded
            | RevocationCode::KeyRetired
            | RevocationCode::CertUserIdInvalid,
        ) => RevocationReach::FromThen,
        _ => RevocationReach::Retroactive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_benign_reasons_are_soft() {
        assert_eq!(
            reach(Some(&RevocationCode::KeySuperseded)),
            RevocationReach::FromThen
        );
        assert_eq!(
            reach(Some(&RevocationCode::KeyRetired)),
            RevocationReach::FromThen
        );
        assert_eq!(
            reach(Some(&RevocationCode::CertUserIdInvalid)),
            RevocationReach::FromThen
        );
        for hard in [
            None,
            Some(&RevocationCode::NoReason),
            Some(&RevocationCode::KeyCompromised),
            Some(&RevocationCode::Private100),
        ] {
            assert_eq!(reach(hard), RevocationReach::Retroactive, "{hard:?}");
        }
    }
}
