//! Scope-keyed contract cases: claim, apply (delta and snapshot), reconcile,
//! maintenance, and release.

use core::time::Duration;
use std::collections::BTreeSet;

use engine_core::sync::{SyncState, SyncUpdate};
use engine_core::write::PendingOutcome;

use crate::apply::{ApplyBatch, DerivedWrite, FtsField, FtsRow, PendingReconciliation};
use crate::error::StoreError;
use crate::lease::ManualClock;
use crate::outbox::PendingOpState;
use crate::store::{Store, StoreRead};

use super::{TestObject, acct, email_scope, lease_request, mailbox_scope, pending_op, pk};

/// A write under a superseded lease is rejected; the winner's data is intact.
pub(super) async fn stale_lease_is_rejected<S: Store + StoreRead>(store: &S, clock: &ManualClock) {
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

/// Replaying an identical batch under the same live lease leaves identical state.
pub(super) async fn replay_is_idempotent<S: Store + StoreRead>(store: &S, _clock: &ManualClock) {
    let account = acct("acct-replay");
    let scope = email_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();

    let update = SyncUpdate::delta(
        vec![TestObject::new("m1", "one"), TestObject::new("m2", "two")],
        vec![],
    );
    let mut derived = DerivedWrite::empty();
    derived.fts.push(FtsRow::new(
        pk("m1"),
        vec![FtsField::new("subject", "hello")],
    ));
    let cursor = SyncState::new("cursor-1");

    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &cursor),
        )
        .await
        .unwrap();
    let keys_once = store.object_keys(&scope).await.unwrap();
    let payload_once = store.object_payload(&scope, &pk("m1")).await.unwrap();
    let state_once = store
        .load_sync_state(account.clone(), &scope)
        .await
        .unwrap();

    // Replay the identical batch under the same still-current lease.
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &cursor),
        )
        .await
        .unwrap();
    assert_eq!(store.object_keys(&scope).await.unwrap(), keys_once);
    assert_eq!(
        store.object_payload(&scope, &pk("m1")).await.unwrap(),
        payload_once
    );
    assert_eq!(
        store
            .load_sync_state(account.clone(), &scope)
            .await
            .unwrap(),
        state_once
    );
    assert_eq!(keys_once, vec![pk("m1"), pk("m2")]);
    assert_eq!(state_once, Some(SyncState::new("cursor-1")));
}

/// A snapshot tombstones exactly the local rows absent from its id set.
pub(super) async fn snapshot_tombstones_only_absent<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-snapshot");
    let scope = email_scope(&account);
    let derived = DerivedWrite::empty();
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();

    let full = SyncUpdate::snapshot(
        vec![
            TestObject::new("a", "A"),
            TestObject::new("b", "B"),
            TestObject::new("c", "C"),
        ],
        [pk("a"), pk("b"), pk("c")]
            .into_iter()
            .collect::<BTreeSet<_>>(),
    );
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&full, &derived, &[], &SyncState::new("snap-1")),
        )
        .await
        .unwrap();
    assert_eq!(
        store.object_keys(&scope).await.unwrap(),
        vec![pk("a"), pk("b"), pk("c")]
    );

    // The next snapshot omits `b`: only `b` is tombstoned, `a`/`c` stay.
    let partial = SyncUpdate::snapshot(
        vec![TestObject::new("a", "A"), TestObject::new("c", "C")],
        [pk("a"), pk("c")].into_iter().collect::<BTreeSet<_>>(),
    );
    let applied = store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&partial, &derived, &[], &SyncState::new("snap-2")),
        )
        .await
        .unwrap();
    assert_eq!(
        store.object_keys(&scope).await.unwrap(),
        vec![pk("a"), pk("c")]
    );
    assert_eq!(applied.tombstoned, 1);
}

/// A reconciliation whose op changed state between planning and apply is skipped,
/// and the incoming object is stored without loss.
pub(super) async fn reconciliation_skips_regressed_op<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-reconcile");
    let scope = email_scope(&account);

    // Claim an op (InFlight) then resolve it Succeeded — it has regressed out of
    // the state the reconciliation will be planned against.
    let op_id = store
        .enqueue_pending_op(account.clone(), pending_op("submit-1", "draft-1"))
        .await
        .unwrap();
    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker", 300), 10)
        .await
        .unwrap();
    store
        .mark_pending_op(
            &claimed[0].lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server-x"),
            },
        )
        .await
        .unwrap();

    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let incoming = SyncUpdate::delta(vec![TestObject::new("m-incoming", "synced")], vec![]);
    let derived = DerivedWrite::empty();
    let reconcile = vec![PendingReconciliation::new(
        op_id,
        PendingOpState::InFlight,
        pk("m-incoming"),
    )];
    let applied = store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&incoming, &derived, &reconcile, &SyncState::new("cursor")),
        )
        .await
        .unwrap();

    // Reconciliation is skipped (the op is no longer InFlight)...
    assert_eq!(applied.reconciled, 0);
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    // ...but the incoming object is stored without loss.
    assert!(
        store
            .object_payload(&scope, &pk("m-incoming"))
            .await
            .unwrap()
            .is_some()
    );
}

/// Container and member scopes are independent units: tombstoning a container in
/// its scope never implicitly touches the member scope (cross-scope cascade is
/// orchestrated per lease, in `engine-sync`).
pub(super) async fn container_and_member_scopes_are_independent<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-container");
    let containers = mailbox_scope(&account);
    let members = email_scope(&account);
    let derived = DerivedWrite::empty();

    // Apply the container scope first (as the orchestrator would).
    let mailboxes = SyncUpdate::snapshot(
        vec![
            TestObject::new("inbox", "Inbox"),
            TestObject::new("archive", "Archive"),
        ],
        [pk("inbox"), pk("archive")]
            .into_iter()
            .collect::<BTreeSet<_>>(),
    );
    let container_claim = store
        .claim_sync_scope(account.clone(), &containers, lease_request("worker", 300))
        .await
        .unwrap();
    store
        .apply_sync_update(
            &container_claim.lease,
            ApplyBatch::new(&mailboxes, &derived, &[], &SyncState::new("mailbox-1")),
        )
        .await
        .unwrap();

    // Then the member scope.
    let emails = SyncUpdate::delta(vec![TestObject::new("e1", "hello")], vec![]);
    let member_claim = store
        .claim_sync_scope(account.clone(), &members, lease_request("worker", 300))
        .await
        .unwrap();
    store
        .apply_sync_update(
            &member_claim.lease,
            ApplyBatch::new(&emails, &derived, &[], &SyncState::new("email-1")),
        )
        .await
        .unwrap();

    // Tombstone a container in the container scope; the member scope is untouched.
    let shrunk = SyncUpdate::snapshot(
        vec![TestObject::new("archive", "Archive")],
        [pk("archive")].into_iter().collect::<BTreeSet<_>>(),
    );
    store
        .apply_sync_update(
            &container_claim.lease,
            ApplyBatch::new(&shrunk, &derived, &[], &SyncState::new("mailbox-2")),
        )
        .await
        .unwrap();

    assert_eq!(
        store.object_keys(&containers).await.unwrap(),
        vec![pk("archive")]
    );
    assert_eq!(store.object_keys(&members).await.unwrap(), vec![pk("e1")]);
}

/// A scope lease is exclusive: a second claim while a live lease is held is
/// rejected, but it succeeds at once after the lease is released.
pub(super) async fn scope_lease_is_exclusive_until_released<S: Store + StoreRead>(
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
pub(super) async fn maintenance_is_lease_gated<S: Store + StoreRead>(
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
