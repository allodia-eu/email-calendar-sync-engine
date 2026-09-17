//! What becomes of a queued **submission**: delivered, parked, settled, withdrawn, or
//! left ambiguous. A send is the one write that must never be repeated, so each outcome
//! is asserted on its own.

use super::{queue_a_send, store_and_clock};
use crate::tests::*;

/// The whole point of the drainer: a send that could not go out stays queued, and the
/// next pass delivers it. Before this, the op was recorded and nothing ever came back.
#[tokio::test]
async fn a_queued_send_goes_out_on_the_next_drain() {
    // One refusal, then the provider is back: an outage the queue rides out.
    let provider = FakeMail::new(vec![], vec![]).failing_sends(1);
    let (store, clock) = store_and_clock();
    queue_a_send(&provider, &store, "send-drain@test.local").await;

    let queued = store.list_pending_ops(account()).await.unwrap();
    assert_eq!(queued.len(), 1, "the send must still be somewhere");

    // The store parked it behind a backoff, so a pass now must leave it alone.
    let early = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert!(early.is_idle(), "a parked op is not due yet");
    assert_eq!(early.deferred, 1);

    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(report.delivered(), 1);
    assert_eq!(report.attempted.len(), 1);
    assert_eq!(report.attempted[0].kind, PendingOpKind::MailSubmit);
    assert_eq!(report.attempted[0].outcome, DrainOutcome::Succeeded);
    // Nothing outstanding: the message went out and the op settled.
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}
/// A failure during a drain parks the op again, counting the attempt, rather than
/// settling it: the send is still coming.
#[tokio::test]
async fn a_drain_parks_a_failure_and_counts_the_attempt() {
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::Submit);
    let (store, clock) = store_and_clock();
    queue_a_send(&provider, &store, "send-park@test.local").await;

    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(report.delivered(), 0);
    assert_eq!(
        report.attempted[0].outcome,
        DrainOutcome::Parked {
            class: FailureClass::RateLimited,
            // One inline attempt, one from this pass.
            attempts: 2,
        }
    );
    assert_eq!(store.list_pending_ops(account()).await.unwrap().len(), 1);
}
/// A delivered message whose copy could not be filed is reported as its own outcome. The
/// op is settled either way: the mail has gone, and re-sending it would be far worse than
/// a missing copy.
#[tokio::test]
async fn a_drain_reports_a_delivered_send_whose_copy_was_not_filed() {
    let provider = FakeMail::new(vec![], vec![])
        .failing(Fault::UnfiledCopy)
        .failing_sends(1);
    let (store, clock) = store_and_clock();
    queue_a_send(&provider, &store, "send-unfiled@test.local").await;

    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(
        report.attempted[0].outcome,
        DrainOutcome::SentNotFiled {
            detail: "APPEND refused: over quota".to_owned(),
        }
    );
    // Counted as delivered, because it was: the recipients have it.
    assert_eq!(report.delivered(), 1);
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}
/// An ambiguous send is parked for confirmation and the drainer must never pick it up
/// again: a blind retry is how a message reaches its recipients twice.
#[tokio::test]
async fn a_drain_never_retries_an_ambiguous_send() {
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::AmbiguousSubmit);
    let (store, clock) = store_and_clock();
    queue_a_send(&provider, &store, "send-ambiguous@test.local").await;

    let queued = store.list_pending_ops(account()).await.unwrap();
    assert_eq!(queued[0].state, PendingOpState::NeedsConfirmation);

    clock.advance(Duration::from_mins(10));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert!(report.is_idle(), "an ambiguous send must not be re-sent");
    assert_eq!(report.deferred, 1);
    // Still parked, and still outstanding: only a confirmation resolves it.
    let after = store.list_pending_ops(account()).await.unwrap();
    assert_eq!(after[0].state, PendingOpState::NeedsConfirmation);
}
/// A send the drainer finds ambiguous is parked for confirmation, not recorded as a
/// retryable failure. The pass that *creates* the ambiguity is the dangerous one: park it
/// wrong and the next pass sends the message again.
#[tokio::test]
async fn a_drain_parks_a_send_it_cannot_call_either_way() {
    // Throttled first, so the send queues; ambiguous on the drain's attempt.
    let provider = FakeMail::new(vec![], vec![]).failing_sends(1);
    let (store, clock) = store_and_clock();
    queue_a_send(&provider, &store, "send-drain-ambiguous@test.local").await;

    let provider = FakeMail::new(vec![], vec![]).failing(Fault::AmbiguousSubmit);
    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(
        report.attempted[0].outcome,
        DrainOutcome::AwaitingConfirmation {
            detail: "post-DATA acknowledgement lost".to_owned(),
        }
    );
    assert_eq!(report.delivered(), 0, "an ambiguous send is not a delivery");
    // Parked, and a second pass must leave it there rather than re-sending.
    clock.advance(Duration::from_mins(30));
    let again = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert!(again.is_idle(), "a parked send must never be re-sent");
}
/// A failure the store will not retry settles during the pass, and is reported as settled
/// rather than as one more park a caller would expect to clear itself.
#[tokio::test]
async fn a_drain_settles_a_failure_no_retry_fixes() {
    let provider = FakeMail::new(vec![], vec![]).failing_sends(1);
    let (store, clock) = store_and_clock();
    queue_a_send(&provider, &store, "send-drain-permanent@test.local").await;

    // The recipient is refused outright on the drain's attempt.
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::PermanentSubmit);
    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(
        report.attempted[0].outcome,
        DrainOutcome::Failed {
            class: FailureClass::Permanent,
        }
    );
    // Off the queue: nothing will attempt it again, and a host can say so.
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}
/// A withdrawn send is gone for good: the drainer must not deliver a message the user
/// deleted from the outbox.
#[tokio::test]
async fn a_cancelled_send_is_never_drained() {
    let provider = FakeMail::new(vec![], vec![]).failing_sends(1);
    let (store, clock) = store_and_clock();
    queue_a_send(&provider, &store, "send-cancelled@test.local").await;

    let queued = store.list_pending_ops(account()).await.unwrap();
    let op = queued[0].id;
    assert_eq!(store.cancel_pending_op(account(), op).await.unwrap(), None);

    clock.advance(Duration::from_mins(1));
    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert!(report.is_idle(), "a withdrawn send must not go out");
    assert_eq!(report.deferred, 0, "a settled op is not even in the queue");
    // Still withdrawn, not delivered: a drain must not resurrect a settled op, and
    // `Cancelled` must not decay into a success the user never asked for.
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Cancelled)
    );
}
