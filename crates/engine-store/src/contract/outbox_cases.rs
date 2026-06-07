//! Outbox contract cases: enqueue (idempotent), claim (dependency/resource
//! filtering, op-lease expiry), and mark.

use core::time::Duration;

use engine_core::write::{PendingOpId, PendingOutcome};

use crate::error::StoreError;
use crate::lease::ManualClock;
use crate::outbox::PendingOpState;
use crate::store::{Store, StoreRead};

use super::{acct, lease_request, pending_op, pk};

/// `mark_pending_op` under an expired op lease is rejected after the op was
/// re-claimed; the new lease succeeds.
pub(super) async fn expired_op_lease_is_rejected<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-op-expiry");
    let op_id = store
        .enqueue_pending_op(account.clone(), pending_op("send-1", "draft-1"))
        .await
        .unwrap();

    let claimed_old = store
        .claim_pending_ops(account.clone(), lease_request("worker-old", 30), 10)
        .await
        .unwrap();
    assert_eq!(claimed_old.len(), 1);
    let old_lease = claimed_old[0].lease.clone();

    clock.advance(Duration::from_secs(90)); // the op lease expires
    let claimed_new = store
        .claim_pending_ops(account.clone(), lease_request("worker-new", 30), 10)
        .await
        .unwrap();
    assert_eq!(claimed_new.len(), 1);
    let new_lease = claimed_new[0].lease.clone();
    assert_ne!(old_lease.token(), new_lease.token());

    let rejected = store
        .mark_pending_op(
            &old_lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server-1"),
            },
        )
        .await
        .expect_err("stale op lease must be rejected");
    assert_eq!(rejected, StoreError::StaleLease);

    store
        .mark_pending_op(
            &new_lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server-1"),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// `claim_pending_ops` never returns an op with unmet `depends_on`, nor two ops
/// sharing a `resource_key`.
pub(super) async fn claim_filters_dependencies_and_resources<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-claim");
    let first_id = store
        .enqueue_pending_op(account.clone(), pending_op("first", "resource-x"))
        .await
        .unwrap();
    let mut dependent = pending_op("second", "resource-y");
    dependent.depends_on.push(first_id);
    let second_id = store
        .enqueue_pending_op(account.clone(), dependent)
        .await
        .unwrap();
    let third_id = store
        .enqueue_pending_op(account.clone(), pending_op("third", "resource-x"))
        .await
        .unwrap();

    // Only the first op runs: the second's dependency is unmet, the third
    // collides on `resource-x`.
    let round_one = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    assert_eq!(
        round_one.iter().map(|l| l.id).collect::<Vec<_>>(),
        vec![first_id]
    );

    // While the first op holds `resource-x` in flight, a second claim returns
    // nothing: the third op is blocked by the busy resource, the second by its
    // still-unmet dependency.
    let blocked = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    assert!(blocked.is_empty());

    store
        .mark_pending_op(
            &round_one[0].lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server"),
            },
        )
        .await
        .unwrap();

    // Now the second (dependency satisfied) and third (resource free) both run.
    let round_two = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    let mut got: Vec<PendingOpId> = round_two.iter().map(|l| l.id).collect();
    got.sort();
    let mut want = vec![second_id, third_id];
    want.sort();
    assert_eq!(got, want);
}

/// Re-enqueuing a duplicate idempotency key returns the original id and creates
/// no second op.
pub(super) async fn enqueue_is_idempotent<S: Store + StoreRead>(store: &S, _clock: &ManualClock) {
    let account = acct("acct-idempotent");
    let first = store
        .enqueue_pending_op(account.clone(), pending_op("dup", "resource"))
        .await
        .unwrap();
    let again = store
        .enqueue_pending_op(account.clone(), pending_op("dup", "resource"))
        .await
        .unwrap();
    assert_eq!(first, again);
    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, first);
}
