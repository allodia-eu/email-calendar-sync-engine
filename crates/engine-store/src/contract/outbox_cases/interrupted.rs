//! Recovering the ops a previous process left in flight.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    write::{PendingOpId, PendingOpKind, PendingOutcome},
};

use super::super::{acct, lease_request, pending_op_of, pk};
use crate::{
    error::StoreError,
    lease::{ManualClock, OpLease},
    outbox::{ClaimRejection, PendingOpClaim, PendingOpState},
    read::StoreRead,
    store::Store,
};

/// Claims `op` for `worker` under a five-minute lease, failing the case if it is refused.
pub(super) async fn claim<S: Store>(
    store: &S,
    account: &engine_core::ids::AccountId,
    op: PendingOpId,
    worker: &str,
) -> OpLease {
    let PendingOpClaim::Leased(leased) = store
        .claim_pending_op(account.clone(), op, lease_request(worker, 300))
        .await
        .unwrap()
    else {
        panic!("{op:?} must be claimable");
    };
    leased.lease
}

/// A process that dies with ops in flight leaves them `InFlight`, and start-up recovery takes
/// each one down the path its own failure would. A send turns on its hand-over record: one that
/// never recorded it cannot have been delivered and is retried at once, one that did awaits
/// confirmation, which no claim takes. Anything else retries as a retryable failure does. The
/// dead workers are fenced, except the right of the attempt that handed a send over to say what
/// became of it, and an op nobody held is untouched.
pub(in crate::contract) async fn an_op_the_previous_process_left_in_flight_is_recovered<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-interrupted");
    let enqueue = |kind, name: &'static str| {
        let account = account.clone();
        async move {
            store
                .enqueue_pending_op(account, pending_op_of(kind, name, name))
                .await
                .unwrap()
        }
    };
    let handed = enqueue(PendingOpKind::MailSubmit, "send-handed-over").await;
    let connecting = enqueue(PendingOpKind::MailSubmit, "send-connecting").await;
    let edit = enqueue(PendingOpKind::MailEdit, "edit-1").await;
    let queued = enqueue(PendingOpKind::MailEdit, "edit-2").await;

    let handed_lease = claim(store, &account, handed, "dead-worker").await;
    store.record_hand_over(&handed_lease).await.unwrap();
    let connecting_lease = claim(store, &account, connecting, "dead-worker").await;
    let edit_lease = claim(store, &account, edit, "dead-worker").await;

    // The leases are still live: at start-up every lease belongs to a process that is gone.
    assert_eq!(store.recover_interrupted_ops().await.unwrap(), 3);
    assert_eq!(
        store.recover_interrupted_ops().await.unwrap(),
        0,
        "recovery changes nothing it has already recovered"
    );

    // The send that was handed over may have gone. It waits for an answer, and no claim
    // reaches it, however long its old lease would have lasted.
    assert_eq!(
        store.pending_op_state(handed).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );
    clock.advance(Duration::from_hours(1));
    assert_eq!(
        store
            .claim_pending_op(account.clone(), handed, lease_request("next-worker", 300))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );

    let rows = store.list_pending_ops(account.clone()).await.unwrap();
    let row = |id| rows.iter().find(|row| row.id == id).expect("listed");
    assert!(
        row(handed)
            .detail
            .as_deref()
            .is_some_and(|detail| !detail.is_empty()),
        "a host needs something to say about the send it cannot vouch for"
    );
    // The send that never got that far is an ordinary retry with the attempt counted.
    assert_eq!(row(connecting).state, PendingOpState::Pending);
    assert_eq!(row(connecting).attempts, 1);
    // The edit is a retryable failure: parked with the attempt counted.
    assert_eq!(row(edit).state, PendingOpState::Pending);
    assert_eq!(row(edit).attempts, 1);
    assert_eq!(row(edit).failure_class, Some(FailureClass::Retryable));
    // The op nobody held is exactly as it was.
    assert_eq!(row(queued).state, PendingOpState::Pending);
    assert_eq!(row(queued).attempts, 0);

    // The dead workers cannot record anything under their old leases, nor hand over.
    let refused = PendingOutcome::Failed {
        class: FailureClass::Permanent,
        retry_after: None,
    };
    for lease in [&connecting_lease, &edit_lease] {
        assert_eq!(
            store.mark_pending_op(lease, refused.clone()).await,
            Err(StoreError::StaleLease)
        );
        assert_eq!(
            store.record_hand_over(lease).await,
            Err(StoreError::StaleLease)
        );
    }
    for op in [connecting, edit] {
        assert!(matches!(
            store
                .claim_pending_op(account.clone(), op, lease_request("next-worker", 300))
                .await
                .unwrap(),
            PendingOpClaim::Leased(_)
        ));
    }

    // The attempt that handed the send over is the one party that knows what became of it,
    // so its answer still lands: a process suspended mid-send that resumes and hears back.
    store
        .mark_pending_op(
            &handed_lease,
            PendingOutcome::Succeeded {
                provider_key: pk("sent-1"),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(handed).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}
