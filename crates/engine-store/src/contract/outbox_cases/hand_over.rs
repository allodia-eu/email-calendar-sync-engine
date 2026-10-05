//! The hand-over record: written under the current lease only, and the one fact the recovery
//! of a dead send turns on, on every path that can meet one.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    ids::AccountId,
    write::{PendingOpId, PendingOpKind, PendingOutcome},
};

use super::{
    super::{acct, lease_request, pending_op_of, pk},
    interrupted::claim,
};
use crate::{
    error::StoreError,
    lease::ManualClock,
    outbox::{ClaimRejection, Confirmation, OpRejection, PendingOpClaim, PendingOpState},
    read::StoreRead,
    store::Store,
};

/// How long a case's lease lasts before it is dead: `claim`'s five minutes, and a second.
const PAST_THE_LEASE: Duration = Duration::from_secs(301);

pub(super) async fn send<S: Store>(store: &S, account: &AccountId, name: &str) -> PendingOpId {
    store
        .enqueue_pending_op(
            account.clone(),
            pending_op_of(PendingOpKind::MailSubmit, name, &format!("draft:{name}")),
        )
        .await
        .unwrap()
}

/// A send whose attempt died, with or without having recorded the hand-over first.
async fn dead_send<S: Store>(
    store: &S,
    clock: &ManualClock,
    account: &AccountId,
    name: &str,
    handed_over: bool,
) -> PendingOpId {
    let op = send(store, account, name).await;
    let lease = claim(store, account, op, "dead-worker").await;
    if handed_over {
        store.record_hand_over(&lease).await.unwrap();
    }
    clock.advance(PAST_THE_LEASE);
    op
}

pub(super) async fn row<S: StoreRead>(
    store: &S,
    account: &AccountId,
    op: PendingOpId,
) -> crate::PendingOpRow {
    store
        .list_pending_ops(account.clone())
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == op)
        .unwrap_or_else(|| panic!("{op:?} is listed"))
}

/// The record is written under the op's current lease and nowhere else: not under a lease a
/// re-claim superseded, and not once the attempt has reported its outcome.
pub(in crate::contract) async fn the_hand_over_is_recorded_under_the_current_lease_only<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-hand-over-fence");
    let op = send(store, &account, "fenced").await;
    let first = claim(store, &account, op, "first").await;
    store.record_hand_over(&first).await.unwrap();
    store.record_hand_over(&first).await.unwrap(); // a second record is a no-op

    // A definitive refusal after it: the server answered, so the record goes with it.
    store
        .mark_pending_op(
            &first,
            PendingOutcome::Failed {
                class: FailureClass::Retryable,
                retry_after: Some(engine_core::time::Duration::ZERO),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.record_hand_over(&first).await,
        Err(StoreError::StaleLease),
        "an attempt that has reported its outcome has nothing left to hand over"
    );

    // The next attempt dies before its own hand-over: the refused attempt's record must not
    // have survived to make it look delivered.
    let second = claim(store, &account, op, "second").await;
    clock.advance(PAST_THE_LEASE);
    assert_eq!(
        row(store, &account, op).await.state,
        PendingOpState::Pending,
        "a definitive refusal clears the hand-over"
    );
    assert_eq!(
        store.record_hand_over(&second).await,
        Ok(()),
        "a lapsed lease nobody recovered is still the op's current one"
    );
    // Recorded, then lapsed: the next claim finds a send that may have gone, and the attempt
    // that recorded it can record nothing more.
    assert_eq!(
        store
            .claim_pending_op(account.clone(), op, lease_request("third", 300))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );
    assert_eq!(
        store.record_hand_over(&second).await,
        Err(StoreError::StaleLease),
        "a recovered attempt cannot record a hand-over"
    );
}

/// A dead attempt that never recorded the hand-over is an ordinary retry, due at once, and
/// every path that can meet it says so: the reads, both claims, and the host's verbs.
pub(in crate::contract) async fn a_dead_send_that_never_handed_over_is_retried_on_every_path<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-dead-unmarked");

    let read = dead_send(store, clock, &account, "read", false).await;
    assert_eq!(
        store.pending_op_state(read).await.unwrap(),
        Some(PendingOpState::Pending)
    );
    let listed = row(store, &account, read).await;
    assert_eq!(listed.state, PendingOpState::Pending);
    assert!(
        listed
            .next_attempt_at
            .is_none_or(|due| due <= clock_now(clock))
    );

    let batch = store
        .claim_pending_ops(account.clone(), lease_request("next", 300), 10)
        .await
        .unwrap();
    assert_eq!(batch.iter().map(|op| op.id).collect::<Vec<_>>(), vec![read]);

    let targeted = dead_send(store, clock, &account, "targeted", false).await;
    assert!(matches!(
        store
            .claim_pending_op(account.clone(), targeted, lease_request("next", 300))
            .await
            .unwrap(),
        PendingOpClaim::Leased(_)
    ));

    // "Send now" on it actually sends: it is due, and the next claim takes it.
    let hurried = dead_send(store, clock, &account, "hurried", false).await;
    assert_eq!(
        store
            .retry_pending_op_now(account.clone(), hurried)
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        store
            .claim_pending_op(account.clone(), hurried, lease_request("next", 300))
            .await
            .unwrap(),
        PendingOpClaim::Leased(_)
    ));

    let withdrawn = dead_send(store, clock, &account, "withdrawn", false).await;
    assert_eq!(
        store
            .cancel_pending_op(account.clone(), withdrawn)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.pending_op_state(withdrawn).await.unwrap(),
        Some(PendingOpState::Cancelled)
    );
}

/// A dead attempt that recorded the hand-over may have delivered the message, and every path
/// that can meet it says so: it awaits confirmation, no claim takes it, neither host verb acts
/// on it, and only a confirmation resolves it.
pub(in crate::contract) async fn a_dead_send_that_handed_over_awaits_confirmation_on_every_path<
    S: Store + StoreRead,
>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-dead-marked");
    let mut ops = Vec::new();
    for name in ["read", "batch", "targeted", "hurried", "withdrawn"] {
        ops.push(dead_send(store, clock, &account, name, true).await);
    }
    let [read, batch, targeted, hurried, withdrawn] = ops[..] else {
        unreachable!()
    };

    assert_eq!(
        store.pending_op_state(read).await.unwrap(),
        Some(PendingOpState::NeedsConfirmation)
    );
    let listed = row(store, &account, read).await;
    assert_eq!(listed.state, PendingOpState::NeedsConfirmation);
    assert!(listed.detail.is_some_and(|detail| !detail.is_empty()));
    assert!(
        listed.payload.get("idempotency").is_some(),
        "the payload stays whole"
    );

    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("next", 300), 10)
        .await
        .unwrap();
    assert!(claimed.is_empty(), "{claimed:?}");
    assert_eq!(
        row(store, &account, batch).await.state,
        PendingOpState::NeedsConfirmation
    );
    assert_eq!(
        store
            .claim_pending_op(account.clone(), targeted, lease_request("next", 300))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );
    assert_eq!(
        store
            .retry_pending_op_now(account.clone(), hurried)
            .await
            .unwrap(),
        Some(OpRejection::AwaitingConfirmation)
    );
    assert_eq!(
        store
            .cancel_pending_op(account.clone(), withdrawn)
            .await
            .unwrap(),
        Some(OpRejection::AwaitingConfirmation)
    );

    // "It was sent" settles it for good.
    assert_eq!(
        store
            .confirm_pending_op(account.clone(), read, Confirmation::Delivered)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.pending_op_state(read).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
    // "It was not" sends it again, at once.
    assert_eq!(
        store
            .confirm_pending_op(account.clone(), batch, Confirmation::NotDelivered)
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        store
            .claim_pending_op(account.clone(), batch, lease_request("next", 300))
            .await
            .unwrap(),
        PendingOpClaim::Leased(_)
    ));
    // A confirmation names an op that awaits one.
    let fresh = send(store, &account, "fresh").await;
    assert_eq!(
        store
            .confirm_pending_op(account.clone(), fresh, Confirmation::Delivered)
            .await
            .unwrap(),
        Some(OpRejection::NotAwaitingConfirmation)
    );
    assert_eq!(
        store
            .confirm_pending_op(account, PendingOpId::new(9_999), Confirmation::Delivered)
            .await
            .unwrap(),
        Some(OpRejection::Unknown)
    );
}

/// Once the host has answered, the attempt that handed the send over has no say: "send it
/// again" starts a new attempt, and a late answer from the old one must not settle it.
pub(in crate::contract) async fn a_confirmation_ends_the_old_attempts_say<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-confirm-fence");
    let op = send(store, &account, "resent").await;
    let old = claim(store, &account, op, "suspended").await;
    store.record_hand_over(&old).await.unwrap();
    clock.advance(PAST_THE_LEASE);
    assert_eq!(
        store
            .confirm_pending_op(account.clone(), op, Confirmation::NotDelivered)
            .await
            .unwrap(),
        None
    );
    let delivered = PendingOutcome::Succeeded {
        provider_key: pk("late"),
    };
    assert_eq!(
        store.mark_pending_op(&old, delivered).await,
        Err(StoreError::StaleLease)
    );
    assert_eq!(
        row(store, &account, op).await.state,
        PendingOpState::Pending
    );
}

fn clock_now(clock: &ManualClock) -> engine_core::time::UtcDateTime {
    crate::lease::Clock::now(clock)
}
