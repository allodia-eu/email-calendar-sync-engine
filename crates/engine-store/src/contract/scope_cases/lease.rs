//! Lease lifecycle cases: fencing on stale writes, claim exclusivity,
//! lease-gated maintenance, and release under a superseded token.

use core::time::Duration;

use engine_core::sync::{SyncState, SyncUpdate};

use super::super::{TestObject, acct, email_scope, lease_request, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite, FtsField, FtsRow},
    error::StoreError,
    lease::ManualClock,
    store::{Store, StoreRead},
};

/// A write under a superseded lease is rejected; the winner's data is intact.
pub(in crate::contract) async fn stale_lease_is_rejected<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-stale");
    let scope = email_scope(&account);
    let derived = DerivedWrite::empty();

    let losing = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-old", 30))
        .await
        .expect("first claim");
    // The old worker is suspended; its lease expires and a new worker re-claims.
    clock.advance(Duration::from_secs(90));
    let winning = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-new", 30))
        .await
        .expect("re-claim after expiry");
    assert_ne!(losing.lease.token(), winning.lease.token());

    let old_objects = SyncUpdate::delta(vec![TestObject::new("m-old", "old")], vec![]);
    let old_cursor = SyncState::new("cursor-old");
    let rejected = store
        .apply_sync_update(
            &losing.lease,
            ApplyBatch::new(&old_objects, &derived, &[], &old_cursor),
        )
        .await
        .expect_err("stale write must be rejected");
    assert_eq!(rejected, StoreError::StaleLease);

    let new_objects = SyncUpdate::delta(vec![TestObject::new("m-new", "new")], vec![]);
    let new_cursor = SyncState::new("cursor-new");
    store
        .apply_sync_update(
            &winning.lease,
            ApplyBatch::new(&new_objects, &derived, &[], &new_cursor),
        )
        .await
        .expect("winning write");

    assert_eq!(store.object_keys(&scope).await.unwrap(), vec![pk("m-new")]);
    assert!(
        store
            .object_payload(&scope, &pk("m-old"))
            .await
            .unwrap()
            .is_none()
    );
}

/// A scope lease is exclusive: a second claim while a live lease is held is
/// rejected, but it succeeds at once after the lease is released.
pub(in crate::contract) async fn scope_lease_is_exclusive_until_released<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-exclusive");
    let scope = email_scope(&account);

    let held = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-a", 300))
        .await
        .unwrap();
    let contended = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-b", 300))
        .await
        .expect_err("a live lease blocks a second claim");
    assert_eq!(contended, StoreError::ScopeHeld);

    // Releasing frees the scope before its TTL; the next claim succeeds at once.
    store.release_sync_scope(held.lease).await.unwrap();
    store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-b", 300))
        .await
        .expect("claim after release");
}

/// `apply_maintenance` is gated by the same scope lease as sync: it succeeds
/// under the current lease and is rejected under a superseded one.
pub(in crate::contract) async fn maintenance_is_lease_gated<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-maintenance");
    let scope = email_scope(&account);

    let current = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-a", 30))
        .await
        .unwrap();
    let mut derived = DerivedWrite::empty();
    derived.fts.push(FtsRow::new(
        pk("m1"),
        vec![FtsField::new("body", "indexed")],
    ));
    store
        .apply_maintenance(&current.lease, &derived)
        .await
        .expect("maintenance under the current lease");

    // After expiry and re-claim, the old lease can no longer write derived rows.
    clock.advance(Duration::from_secs(90));
    store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-b", 30))
        .await
        .unwrap();
    let rejected = store
        .apply_maintenance(&current.lease, &derived)
        .await
        .expect_err("stale maintenance must be rejected");
    assert_eq!(rejected, StoreError::StaleLease);
}

/// Releasing under a superseded lease is a no-op: it must not free a scope that a
/// newer lease now holds.
pub(in crate::contract) async fn release_with_stale_token_is_noop<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-release-stale");
    let scope = email_scope(&account);

    let stale = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-old", 30))
        .await
        .unwrap();
    // The old lease expires; a new worker re-claims and bumps the generation.
    clock.advance(Duration::from_secs(90));
    store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-new", 30))
        .await
        .unwrap();

    // Releasing the stale lease must not free the scope the live lease holds.
    store.release_sync_scope(stale.lease).await.unwrap();
    let contended = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-other", 30))
        .await
        .expect_err("the live new lease still holds the scope");
    assert_eq!(contended, StoreError::ScopeHeld);
}
