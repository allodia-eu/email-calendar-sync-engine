//! Forgetting a scope: what a folder that has left its account's folder list leaves behind.

use core::time::Duration;

use engine_core::sync::{SyncState, SyncUpdate};

use super::super::{TestObject, acct, email_scope, lease_request, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite, FtsField, FtsRow},
    error::StoreError,
    lease::ManualClock,
    read::StoreRead,
    store::Store,
};

/// Forgetting a scope drops its objects, its derived rows and its cursor, so the next claim
/// starts from nothing, and leaves every other scope alone.
pub(in crate::contract) async fn forget_scope_drops_objects_rows_and_cursor<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-forget");
    let scope = email_scope(&account);
    let other = email_scope(&acct("acct-forget-other"));

    for held in [&scope, &other] {
        let claim = store
            .claim_sync_scope(held.account().clone(), held, lease_request("worker", 30))
            .await
            .unwrap();
        let objects = SyncUpdate::delta(vec![TestObject::new("m1", "kept")], vec![]);
        let mut derived = DerivedWrite::empty();
        derived.fts.push(FtsRow::new(
            pk("m1"),
            vec![FtsField::new("body", "indexed")],
        ));
        let cursor = SyncState::new("cursor-1");
        store
            .apply_sync_update(
                &claim.lease,
                ApplyBatch::new(&objects, &derived, &[], &cursor),
            )
            .await
            .unwrap();
        store.release_sync_scope(claim.lease).await.unwrap();
    }
    assert_eq!(
        store.index_row_counts(&scope, &pk("m1")).await.unwrap().fts,
        1
    );

    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 30))
        .await
        .unwrap();
    store
        .forget_scope(claim.lease)
        .await
        .expect("forget under the live lease");

    assert!(store.object_keys(&scope).await.unwrap().is_empty());
    assert!(
        store
            .index_row_counts(&scope, &pk("m1"))
            .await
            .unwrap()
            .is_empty(),
        "the derived rows go with the objects",
    );
    assert!(
        store
            .load_sync_state(account.clone(), &scope)
            .await
            .unwrap()
            .is_none()
    );
    // The lease went with it: the scope is claimable at once, and from no cursor.
    let reclaimed = store
        .claim_sync_scope(account, &scope, lease_request("worker-next", 30))
        .await
        .expect("a forgotten scope is free");
    assert!(reclaimed.state.is_none());

    assert_eq!(store.object_keys(&other).await.unwrap(), vec![pk("m1")]);
    assert!(
        store
            .load_sync_state(other.account().clone(), &other)
            .await
            .unwrap()
            .is_some()
    );
}

/// A superseded lease forgets nothing: the worker that re-claimed the scope keeps what it wrote.
pub(in crate::contract) async fn forget_scope_under_a_stale_lease_is_rejected<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-forget-stale");
    let scope = email_scope(&account);

    let stale = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-old", 30))
        .await
        .unwrap();
    clock.advance(Duration::from_secs(90));
    let live = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker-new", 30))
        .await
        .unwrap();
    let objects = SyncUpdate::delta(vec![TestObject::new("m1", "kept")], vec![]);
    let cursor = SyncState::new("cursor-live");
    store
        .apply_sync_update(
            &live.lease,
            ApplyBatch::new(&objects, &DerivedWrite::empty(), &[], &cursor),
        )
        .await
        .unwrap();

    let rejected = store
        .forget_scope(stale.lease)
        .await
        .expect_err("a stale lease must not forget");
    assert_eq!(rejected, StoreError::StaleLease);
    assert_eq!(store.object_keys(&scope).await.unwrap(), vec![pk("m1")]);
}
