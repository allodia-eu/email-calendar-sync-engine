//! What keeps a send's attempt its own while it runs, and what keeps a send in front of the
//! user once the outbox gives up on it.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    write::{PendingOpKind, PendingOutcome},
};

use super::{
    super::{acct, lease_request, pending_op_of},
    hand_over::{row, send},
    interrupted::claim,
};
use crate::{
    error::StoreError,
    lease::ManualClock,
    outbox::{ClaimRejection, OpRejection, PendingOpClaim, PendingOpState},
    read::StoreRead,
    store::Store,
};

/// A worker that renews its lease keeps a slow attempt its own past any fixed TTL; once it
/// stops renewing, the lease lapses like any other and the op is recovered.
pub(in crate::contract) async fn a_renewed_lease_keeps_a_slow_attempt_its_own<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-renewal");
    let op = send(store, &account, "slow").await;
    let PendingOpClaim::Leased(leased) = store
        .claim_pending_op(account.clone(), op, lease_request("slow-worker", 30))
        .await
        .unwrap()
    else {
        panic!("a fresh op must be claimable");
    };
    let lease = leased.lease;

    // Ten minutes of renewing every ten seconds, against a thirty-second lease.
    for _ in 0..60 {
        clock.advance(Duration::from_secs(10));
        store
            .renew_pending_op_lease(&lease, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            store
                .claim_pending_op(account.clone(), op, lease_request("rival", 30))
                .await
                .unwrap(),
            PendingOpClaim::Refused(ClaimRejection::Busy)
        );
        assert!(
            store
                .claim_pending_ops(account.clone(), lease_request("rival", 30), 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            row(store, &account, op).await.state,
            PendingOpState::InFlight
        );
    }
    // Still its own: it can record the hand-over and its outcome.
    store.record_hand_over(&lease).await.unwrap();

    // It stops renewing. Thirty seconds later the attempt is dead and recovered.
    clock.advance(Duration::from_secs(31));
    assert_eq!(
        row(store, &account, op).await.state,
        PendingOpState::NeedsConfirmation
    );
    assert_eq!(
        store
            .claim_pending_op(account.clone(), op, lease_request("rival", 30))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );
    assert_eq!(
        store
            .renew_pending_op_lease(&lease, Duration::from_secs(30))
            .await,
        Err(StoreError::StaleLease),
        "a recovered attempt cannot take its lease back"
    );
}

/// A send the outbox could not deliver stays in front of the user, payload and all, until
/// they send it again or withdraw it; a transient failure never settles one on a count.
pub(in crate::contract) async fn a_send_that_failed_stays_listed_until_the_host_acts<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-failed-send");
    let op = send(store, &account, "refused").await;

    // Thirty transient failures: still queued, never given up on.
    for _ in 0..30 {
        let lease = claim(store, &account, op, "worker").await;
        store
            .mark_pending_op(
                &lease,
                PendingOutcome::Failed {
                    class: FailureClass::Retryable,
                    retry_after: None,
                },
            )
            .await
            .unwrap();
        clock.advance(Duration::from_hours(1));
    }
    let parked = row(store, &account, op).await;
    assert_eq!(parked.state, PendingOpState::Pending);
    assert_eq!(parked.attempts, 30);

    // A refusal no retry fixes settles it, and it stays listed with what it would send.
    let lease = claim(store, &account, op, "worker").await;
    store
        .mark_pending_op(
            &lease,
            PendingOutcome::Failed {
                class: FailureClass::Permanent,
                retry_after: None,
            },
        )
        .await
        .unwrap();
    let failed = row(store, &account, op).await;
    assert_eq!(failed.state, PendingOpState::Failed);
    assert_eq!(failed.failure_class, Some(FailureClass::Permanent));
    assert_eq!(
        failed.payload.get("idempotency").and_then(|v| v.as_str()),
        Some("refused")
    );

    // "Send again": due at once.
    assert_eq!(
        store
            .retry_pending_op_now(account.clone(), op)
            .await
            .unwrap(),
        None
    );
    let lease = claim(store, &account, op, "worker").await;
    store
        .mark_pending_op(
            &lease,
            PendingOutcome::Failed {
                class: FailureClass::Authentication,
                retry_after: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(row(store, &account, op).await.state, PendingOpState::Failed);

    // Withdrawn: gone from the queue, and never attempted.
    assert_eq!(
        store.cancel_pending_op(account.clone(), op).await.unwrap(),
        None
    );
    assert!(
        store
            .list_pending_ops(account.clone())
            .await
            .unwrap()
            .iter()
            .all(|row| row.id != op)
    );
    assert_eq!(
        store
            .retry_pending_op_now(account.clone(), op)
            .await
            .unwrap(),
        Some(OpRejection::Settled)
    );

    // Any other write that settles leaves the queue, as it always has.
    let edit = store
        .enqueue_pending_op(
            account.clone(),
            pending_op_of(PendingOpKind::MailEdit, "edit", "mail:edit"),
        )
        .await
        .unwrap();
    let lease = claim(store, &account, edit, "worker").await;
    store
        .mark_pending_op(
            &lease,
            PendingOutcome::Failed {
                class: FailureClass::Permanent,
                retry_after: None,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .list_pending_ops(account.clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.retry_pending_op_now(account, edit).await.unwrap(),
        Some(OpRejection::Settled)
    );
}
