//! The queue half: a retryable failure that comes back, an attempt bound that
//! finally settles it, withdrawal, and the read that lets a host see any of it.

use core::time::Duration;

use engine_core::{error::FailureClass, write::PendingOutcome};

use super::super::{acct, lease_request, pending_op, pk};
use crate::{
    lease::ManualClock,
    outbox::{CancelRejection, ClaimRejection, MAX_ATTEMPTS, PendingOpClaim, PendingOpState},
    read::StoreRead,
    store::Store,
};

/// A retryable failure does not settle the op: it parks with a raised attempt count,
/// refuses a claim until its backoff elapses, and is claimable again after it.
///
/// This is the difference between an outbox and a log of things that did not happen.
pub(in crate::contract) async fn a_retryable_failure_comes_back_when_its_backoff_elapses<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-retry-parks");
    let op = store
        .enqueue_pending_op(account.clone(), pending_op("retry-1", "res-retry"))
        .await
        .unwrap();

    let claimed = store
        .claim_pending_op(account.clone(), op, lease_request("worker", 300))
        .await
        .unwrap();
    let PendingOpClaim::Leased(leased) = claimed else {
        panic!("a fresh op must be claimable");
    };
    store
        .mark_pending_op(
            &leased.lease,
            PendingOutcome::Failed {
                class: FailureClass::Retryable,
                // No provider hint, so the store derives the delay: 30s for a first
                // failure (`retry_delay`).
                retry_after: None,
            },
        )
        .await
        .unwrap();

    // Parked, not settled: still `Pending`, and the attempt is on the record.
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Pending)
    );
    let row = one_row(store, &account).await;
    assert_eq!(row.attempts, 1);
    assert_eq!(row.failure_class, Some(FailureClass::Retryable));
    assert!(row.next_attempt_at.is_some());

    // Before the backoff elapses the claim is refused, and says so as a backoff
    // rather than as a settled op: the caller must not conclude it is finished.
    assert_eq!(
        store
            .claim_pending_op(account.clone(), op, lease_request("worker", 300))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Backoff)
    );
    // The batch claim skips it for the same reason, rather than leasing it early.
    assert!(
        store
            .claim_pending_ops(account.clone(), lease_request("drainer", 300), 10)
            .await
            .unwrap()
            .is_empty()
    );

    clock.advance(Duration::from_secs(31));
    let after = store
        .claim_pending_op(account.clone(), op, lease_request("worker", 300))
        .await
        .unwrap();
    let PendingOpClaim::Leased(again) = after else {
        panic!("a parked retry must be claimable once its backoff elapses");
    };
    assert_eq!(again.id, op);
    // Re-claiming fences the previous lease, exactly as a dead lease does.
    assert_ne!(again.lease.token(), leased.lease.token());
}

/// The attempt bound is real: a failure that keeps repeating eventually settles as
/// `Failed` instead of retrying for ever.
pub(in crate::contract) async fn a_retryable_failure_settles_once_its_attempts_run_out<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-retry-bound");
    let op = store
        .enqueue_pending_op(account.clone(), pending_op("retry-2", "res-bound"))
        .await
        .unwrap();

    for attempt in 1..=MAX_ATTEMPTS {
        let claimed = store
            .claim_pending_op(account.clone(), op, lease_request("worker", 300))
            .await
            .unwrap();
        let PendingOpClaim::Leased(leased) = claimed else {
            panic!("attempt {attempt} of {MAX_ATTEMPTS} must be claimable");
        };
        store
            .mark_pending_op(
                &leased.lease,
                PendingOutcome::Failed {
                    class: FailureClass::Retryable,
                    retry_after: None,
                },
            )
            .await
            .unwrap();
        // Past the cap, so this clears any backoff the store derived.
        clock.advance(Duration::from_hours(1));
    }

    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Failed)
    );
    // Settled, so it leaves the queue read: nothing is still outstanding.
    assert!(
        store
            .list_pending_ops(account.clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .claim_pending_op(account, op, lease_request("worker", 300))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );
}

/// A cancelled op is never attempted again, and a cancel is refused rather than
/// silently ignored when the op is not in a state that can be withdrawn.
pub(in crate::contract) async fn a_cancelled_op_is_never_attempted<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-cancel");
    let queued = store
        .enqueue_pending_op(account.clone(), pending_op("cancel-1", "res-cancel"))
        .await
        .unwrap();

    assert_eq!(
        store
            .cancel_pending_op(account.clone(), queued)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.pending_op_state(queued).await.unwrap(),
        Some(PendingOpState::Cancelled)
    );
    assert_eq!(
        store
            .claim_pending_op(account.clone(), queued, lease_request("worker", 300))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );
    // Cancelling twice is refused, not a second withdrawal.
    assert_eq!(
        store
            .cancel_pending_op(account.clone(), queued)
            .await
            .unwrap(),
        Some(CancelRejection::Settled)
    );

    // An op under a live lease may be mid-round-trip, so it cannot be withdrawn:
    // claiming to have stopped a send that already left would be a lie.
    let live = store
        .enqueue_pending_op(account.clone(), pending_op("cancel-2", "res-live"))
        .await
        .unwrap();
    let claimed = store
        .claim_pending_op(account.clone(), live, lease_request("worker", 300))
        .await
        .unwrap();
    let PendingOpClaim::Leased(leased) = claimed else {
        panic!("a fresh op must be claimable");
    };
    assert_eq!(
        store
            .cancel_pending_op(account.clone(), live)
            .await
            .unwrap(),
        Some(CancelRejection::InFlight)
    );

    // Once that lease dies the worker holding it is gone, so the op can be withdrawn.
    clock.advance(Duration::from_mins(10));
    assert_eq!(
        store
            .cancel_pending_op(account.clone(), live)
            .await
            .unwrap(),
        None
    );
    // And the fence moved, so the old worker cannot resolve what was withdrawn.
    assert!(
        store
            .mark_pending_op(
                &leased.lease,
                PendingOutcome::Succeeded {
                    provider_key: pk("server-late"),
                },
            )
            .await
            .is_err()
    );
}

/// The queue read enumerates what has not settled, in enqueue order, carrying enough
/// for a host to say what is waiting and why. Settled rows are excluded: they are kept
/// for ever as the idempotency record, and none of them is outstanding.
pub(in crate::contract) async fn a_queue_read_lists_what_has_not_settled<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-queue-read");
    let other = acct("acct-queue-read-other");
    assert!(
        store
            .list_pending_ops(account.clone())
            .await
            .unwrap()
            .is_empty()
    );

    let first = store
        .enqueue_pending_op(account.clone(), pending_op("q-1", "res-q1"))
        .await
        .unwrap();
    let second = store
        .enqueue_pending_op(account.clone(), pending_op("q-2", "res-q2"))
        .await
        .unwrap();
    // Another account's outbox is not this one's.
    store
        .enqueue_pending_op(other.clone(), pending_op("q-other", "res-q1"))
        .await
        .unwrap();

    let rows = store.list_pending_ops(account.clone()).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![first, second]
    );
    assert_eq!(rows[0].idempotency_key.as_str(), "q-1");
    assert_eq!(rows[0].resource_key.as_str(), "res-q1");
    assert_eq!(rows[0].state, PendingOpState::Pending);
    assert_eq!(rows[0].attempts, 0);
    assert_eq!(rows[0].failure_class, None);
    // The payload comes back, because a host renders the queued write from it.
    assert_eq!(rows[0].payload, pending_op("q-1", "res-q1").payload);

    // An in-flight op is still outstanding and stays listed.
    let claimed = store
        .claim_pending_op(account.clone(), first, lease_request("worker", 300))
        .await
        .unwrap();
    let PendingOpClaim::Leased(leased) = claimed else {
        panic!("a fresh op must be claimable");
    };
    let rows = store.list_pending_ops(account.clone()).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].state, PendingOpState::InFlight);

    // A settled one drops out.
    store
        .mark_pending_op(
            &leased.lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server-q1"),
            },
        )
        .await
        .unwrap();
    let rows = store.list_pending_ops(account).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![second]);
}

/// The account's single outstanding row, for a case that enqueued exactly one.
async fn one_row<S: StoreRead>(
    store: &S,
    account: &engine_core::ids::AccountId,
) -> crate::outbox::PendingOpRow {
    let mut rows = store.list_pending_ops(account.clone()).await.unwrap();
    assert_eq!(rows.len(), 1, "case expects exactly one outstanding op");
    rows.remove(0)
}
