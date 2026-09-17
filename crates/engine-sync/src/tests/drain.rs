//! Drainer tests: the pass that attempts what the inline drivers left behind.
//!
//! The case these exist for is the one issue #60 opens with: a write attempted with no
//! network, which used to be recorded and then forgotten. Uses the shared fakes and
//! helpers from the parent module via `use super::*`.

use super::*;

/// The message a queued edit targets.
fn target() -> ProviderKey {
    ProviderKey::new("imap:v1:u42@INBOX").unwrap()
}

/// A store plus the handle that moves its clock: `ManualClock` shares one instant, so
/// the copy held here advances the one the store reads.
fn store_and_clock() -> (SqliteStore<ManualClock>, ManualClock) {
    let clock = clock();
    (SqliteStore::open_in_memory(clock.clone()).unwrap(), clock)
}

/// Queues a send by letting one inline submission fail against `provider`.
async fn queue_a_send(provider: &FakeMail, store: &SqliteStore<ManualClock>, id: &str) {
    submit_mail(
        provider,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft(id),
    )
    .await
    .expect_err("this submission is meant to fail and leave the send queued");
}

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

/// A drain must not lease an op it cannot dispatch. Leasing one and walking away holds its
/// resource for the whole lease, which is the failure #202 removed from the inline path.
#[tokio::test]
async fn a_drain_leaves_a_kind_it_cannot_dispatch_unleased() {
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();
    // A calendar patch: its provider call needs the base event beside the payload, so
    // this pass cannot run it.
    store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("cal-1").unwrap(),
                PendingOpKind::CalendarPatch,
                ResourceKey::new("event:uid-1").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();

    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert!(report.is_idle());
    assert_eq!(report.deferred, 1);
    // Untouched: still Pending, never leased, no attempt recorded.
    let rows = store.list_pending_ops(account()).await.unwrap();
    assert_eq!(rows[0].state, PendingOpState::Pending);
    assert_eq!(rows[0].attempts, 0);
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

/// A queued mail **edit** drains too: the archive that could not reach the server is the
/// symptom this whole line of work started from.
#[tokio::test]
async fn a_queued_archive_reaches_the_server_on_the_next_drain() {
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::WriteGuard);
    let (store, _clock) = store_and_clock();
    let archive = MailEdit::move_to(target(), MailboxId::try_from("Archive").unwrap());
    let acct = account();
    edit_mail(
        &provider,
        &store,
        &acct,
        worker(),
        Duration::from_mins(1),
        "edit:u42:archive",
        &archive,
    )
    .await
    .expect_err("the guarded edit is meant to fail and stay queued");

    // A conflict is not retryable, so it settled rather than queueing: the drainer has
    // nothing to do, and says so.
    let rows = store.list_pending_ops(acct.clone()).await.unwrap();
    assert!(rows.is_empty(), "a conflict settles; it does not queue");

    // The retryable shape of the same write does queue, and does drain.
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();
    store
        .enqueue_pending_op(
            acct.clone(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:archive").unwrap(),
                PendingOpKind::MailEdit,
                ResourceKey::new(format!("mail:{}", target().as_str())).unwrap(),
                serde_json::to_value(&archive).unwrap(),
            ),
        )
        .await
        .unwrap();

    let report = drain_outbox(&provider, &store, &acct, worker(), Duration::from_mins(1))
        .await
        .unwrap();
    assert_eq!(report.attempted.len(), 1);
    assert_eq!(report.attempted[0].kind, PendingOpKind::MailEdit);
    assert_eq!(report.attempted[0].outcome, DrainOutcome::Succeeded);
    assert!(store.list_pending_ops(acct).await.unwrap().is_empty());
}
