//! Apply-path cases: streaming pages, replay idempotency, snapshot
//! tombstoning, reconciliation, and scope independence.

use std::collections::BTreeSet;

use engine_core::{
    sync::{SyncState, SyncUpdate},
    write::{PendingOpId, PendingOutcome},
};

use super::super::{TestObject, acct, email_scope, lease_request, mailbox_scope, pending_op, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite, FtsField, FtsRow, PendingReconciliation},
    lease::ManualClock,
    outbox::PendingOpState,
    store::{Store, StoreRead},
};

/// A streaming page (`next_state == None`) applies its objects but **leaves the
/// cursor unchanged**; a later `Some(cursor)` advances it. This is the primitive
/// that lets a paged fetch commit each page visibly without prematurely marking
/// the scope synced (a crash mid-stream re-syncs from the prior cursor).
pub(in crate::contract) async fn streaming_page_keeps_cursor<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-stream");
    let scope = email_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let derived = DerivedWrite::empty();

    // Page 1 advances the cursor to c1.
    let page1 = SyncUpdate::delta(vec![TestObject::new("m1", "one")], vec![]);
    let c1 = SyncState::new("c1");
    store
        .apply_sync_update(&claim.lease, ApplyBatch::new(&page1, &derived, &[], &c1))
        .await
        .unwrap();

    // Page 2 is additive with the cursor held (None).
    let page2 = SyncUpdate::delta(vec![TestObject::new("m2", "two")], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::with_cursor(&page2, &derived, &[], None),
        )
        .await
        .unwrap();

    // Both objects are present, but the cursor is still c1 — page 2 did not advance it.
    assert_eq!(store.object_keys(&scope).await.unwrap().len(), 2);
    assert_eq!(
        store
            .load_sync_state(account.clone(), &scope)
            .await
            .unwrap(),
        Some(c1)
    );

    // A final apply advances the cursor to c2.
    let empty: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![]);
    let c2 = SyncState::new("c2");
    store
        .apply_sync_update(&claim.lease, ApplyBatch::new(&empty, &derived, &[], &c2))
        .await
        .unwrap();
    assert_eq!(
        store.load_sync_state(account, &scope).await.unwrap(),
        Some(c2)
    );
}

/// Replaying an identical batch under the same live lease leaves identical state.
pub(in crate::contract) async fn replay_is_idempotent<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
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
pub(in crate::contract) async fn snapshot_tombstones_only_absent<S: Store + StoreRead>(
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
pub(in crate::contract) async fn reconciliation_skips_regressed_op<S: Store + StoreRead>(
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
pub(in crate::contract) async fn container_and_member_scopes_are_independent<
    S: Store + StoreRead,
>(
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

/// A reconciliation whose op is still in its expected state resolves the op to
/// `Succeeded` inside the apply transaction; one naming an unknown op is skipped.
/// Either way the incoming object is stored (the success counterpart to
/// [`reconciliation_skips_regressed_op`]).
pub(in crate::contract) async fn reconciliation_resolves_matching_op<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-reconcile-ok");
    let scope = email_scope(&account);

    // Claim an op into flight — the state the reconciliation will expect.
    let op_id = store
        .enqueue_pending_op(account.clone(), pending_op("submit-ok", "draft-ok"))
        .await
        .unwrap();
    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker", 300), 10)
        .await
        .unwrap();
    assert_eq!(claimed[0].id, op_id);

    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();
    let incoming = SyncUpdate::delta(vec![TestObject::new("m-ok", "synced")], vec![]);
    let derived = DerivedWrite::empty();
    let reconcile = vec![
        PendingReconciliation::new(op_id, PendingOpState::InFlight, pk("m-ok")),
        // An unknown op is skipped, not an error.
        PendingReconciliation::new(
            PendingOpId::new(9_999),
            PendingOpState::InFlight,
            pk("m-ok"),
        ),
    ];
    let applied = store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(
                &incoming,
                &derived,
                &reconcile,
                &SyncState::new("cursor-ok"),
            ),
        )
        .await
        .unwrap();

    assert_eq!(applied.reconciled, 1);
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    assert!(
        store
            .object_payload(&scope, &pk("m-ok"))
            .await
            .unwrap()
            .is_some()
    );
}
