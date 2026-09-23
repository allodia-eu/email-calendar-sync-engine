//! Recovering the ops a previous process left in flight.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    write::{PendingOpKind, PendingOutcome},
};

use super::super::{acct, lease_request, pending_op_of};
use crate::{
    error::StoreError,
    lease::ManualClock,
    outbox::{ClaimRejection, PendingOpClaim, PendingOpState},
    read::StoreRead,
    store::Store,
};

/// A process that dies with ops in flight leaves them `InFlight`, and an `InFlight` op whose
/// lease has lapsed is claimable: a send cut off after the server took it would go out again.
/// Recovery takes each one down the path its own failure would: a send whose outcome is
/// unknown awaits confirmation, which no claim takes, and anything else retries as a
/// retryable failure does. The dead worker is fenced, and an op nobody held is untouched.
pub(in crate::contract) async fn an_op_the_previous_process_left_in_flight_is_recovered<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-interrupted");
    let send = store
        .enqueue_pending_op(
            account.clone(),
            pending_op_of(PendingOpKind::MailSubmit, "send-1", "draft:send-1"),
        )
        .await
        .unwrap();
    let edit = store
        .enqueue_pending_op(
            account.clone(),
            pending_op_of(PendingOpKind::MailEdit, "edit-1", "mail:edit-1"),
        )
        .await
        .unwrap();
    let queued = store
        .enqueue_pending_op(
            account.clone(),
            pending_op_of(PendingOpKind::MailEdit, "edit-2", "mail:edit-2"),
        )
        .await
        .unwrap();

    let mut dead_worker = Vec::new();
    for op in [send, edit] {
        let PendingOpClaim::Leased(leased) = store
            .claim_pending_op(account.clone(), op, lease_request("dead-worker", 300))
            .await
            .unwrap()
        else {
            panic!("a fresh op must be claimable");
        };
        dead_worker.push(leased.lease);
    }

    // The leases are still live: at start-up every lease belongs to a process that is gone.
    assert_eq!(store.recover_interrupted_ops().await.unwrap(), 2);
    assert_eq!(
        store.recover_interrupted_ops().await.unwrap(),
        0,
        "recovery changes nothing it has already recovered"
    );

    // The send may have gone. It waits for an answer, and no claim reaches it, however
    // long its old lease would have lasted.
    assert_eq!(
        store.pending_op_state(send).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );
    clock.advance(Duration::from_hours(1));
    assert_eq!(
        store
            .claim_pending_op(account.clone(), send, lease_request("next-worker", 300))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );

    // The edit is a retryable failure: parked with the attempt counted, then claimable.
    let rows = store.list_pending_ops(account.clone()).await.unwrap();
    let edit_row = rows
        .iter()
        .find(|row| row.id == edit)
        .expect("the edit is listed");
    assert_eq!(edit_row.state, PendingOpState::Pending);
    assert_eq!(edit_row.attempts, 1);
    assert_eq!(edit_row.failure_class, Some(FailureClass::Retryable));
    let send_row = rows
        .iter()
        .find(|row| row.id == send)
        .expect("the send is listed");
    assert!(
        send_row
            .detail
            .as_deref()
            .is_some_and(|detail| !detail.is_empty()),
        "a host needs something to say about the send it cannot vouch for"
    );
    assert!(matches!(
        store
            .claim_pending_op(account.clone(), edit, lease_request("next-worker", 300))
            .await
            .unwrap(),
        PendingOpClaim::Leased(_)
    ));

    // The op nobody held is exactly as it was.
    let queued_row = rows.iter().find(|row| row.id == queued).expect("listed");
    assert_eq!(queued_row.state, PendingOpState::Pending);
    assert_eq!(queued_row.attempts, 0);

    // The dead worker cannot record anything under its old leases.
    for lease in &dead_worker {
        assert_eq!(
            store
                .mark_pending_op(
                    lease,
                    PendingOutcome::Failed {
                        class: FailureClass::Permanent,
                        retry_after: None,
                    }
                )
                .await,
            Err(StoreError::StaleLease)
        );
    }
}
