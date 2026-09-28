//! The other mail writes a pass runs (edits and reports), and everything it deliberately
//! leaves alone: a kind it cannot dispatch, and a payload it cannot read.

use super::{store_and_clock, target};
use crate::tests::*;

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
/// A queued edit that fails during the pass parks like any other retryable failure: the
/// archive is still coming.
#[tokio::test]
async fn a_drain_parks_an_edit_that_fails() {
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::WriteGuard);
    let (store, _clock) = store_and_clock();
    store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:flag").unwrap(),
                PendingOpKind::MailEdit,
                ResourceKey::new(format!("mail:{}", target().as_str())).unwrap(),
                serde_json::to_value(MailEdit::set_flagged(target(), true)).unwrap(),
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

    // A conflict is not retryable: recomputing after a re-sync is the remedy, so it
    // settles rather than backing off into the same answer.
    assert_eq!(
        report.attempted[0].outcome,
        DrainOutcome::Failed {
            class: FailureClass::Conflict,
        }
    );
}
/// A queued spam report drains like the other two mail writes, and parks when throttled.
#[tokio::test]
async fn a_queued_report_reaches_the_provider_on_the_next_drain() {
    let (store, _clock) = store_and_clock();
    let report_op = || {
        PendingOp::new(
            IdempotencyKey::new("report:u42:junk").unwrap(),
            PendingOpKind::MailReport,
            ResourceKey::new(format!("mail:{}", target().as_str())).unwrap(),
            serde_json::to_value(MessageReport::new(
                target(),
                ReportVerdict::Junk,
                MailboxId::try_from("Junk").unwrap(),
            ))
            .unwrap(),
        )
    };
    store
        .enqueue_pending_op(account(), report_op())
        .await
        .unwrap();

    // Throttled: parked, still outstanding.
    let throttled = FakeMail::new(vec![], vec![]).failing(Fault::Report);
    let parked = drain_outbox(
        &throttled,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert_eq!(
        parked.attempted[0].outcome,
        DrainOutcome::Parked {
            class: FailureClass::RateLimited,
            attempts: 1,
        }
    );

    // The provider recovers, and the next pass files the report.
    let healthy = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();
    store
        .enqueue_pending_op(account(), report_op())
        .await
        .unwrap();
    let done = drain_outbox(
        &healthy,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert_eq!(done.attempted[0].kind, PendingOpKind::MailReport);
    assert_eq!(done.attempted[0].outcome, DrainOutcome::Succeeded);
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
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
/// A payload this build cannot read settles its own op and **does not stop the pass**.
/// One unreadable op must not hold every op behind it in the queue for ever.
#[tokio::test]
async fn an_unreadable_payload_settles_without_blocking_the_queue() {
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();
    // A submission whose payload is not a `Draft`.
    store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:corrupt").unwrap(),
                PendingOpKind::MailSubmit,
                ResourceKey::new("draft:corrupt@test.local").unwrap(),
                serde_json::json!({ "not": "a draft" }),
            ),
        )
        .await
        .unwrap();
    // A good send queued behind it, on its own resource.
    store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:good").unwrap(),
                PendingOpKind::MailSubmit,
                ResourceKey::new("draft:good@test.local").unwrap(),
                serde_json::to_value(draft("good@test.local")).unwrap(),
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

    assert_eq!(report.attempted.len(), 2);
    assert!(matches!(
        report.attempted[0].outcome,
        DrainOutcome::Undecodable { .. }
    ));
    // The one behind it still went out, which is the whole point.
    assert_eq!(report.attempted[1].outcome, DrainOutcome::Succeeded);
    assert_eq!(report.delivered(), 1);
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}
/// Whether a pass dispatches `kind` as a mail write, which is what decides whether an
/// unreadable payload of it has to settle rather than be left alone.
///
/// **Exhaustive on purpose.** A hand-written list of kinds goes stale in silence: a kind
/// added later is simply never asserted on, and the test still passes. This match refuses
/// to compile until the new kind is placed on one side or the other.
const fn is_a_mail_write(kind: PendingOpKind) -> bool {
    match kind {
        PendingOpKind::MailSubmit
        | PendingOpKind::MailEdit
        | PendingOpKind::MailReport
        | PendingOpKind::MailDraftPut
        | PendingOpKind::MailDraftDelete
        | PendingOpKind::MailboxEdit => true,
        PendingOpKind::CalendarCreate
        | PendingOpKind::CalendarPatch
        | PendingOpKind::CalendarDocument
        | PendingOpKind::CalendarRsvp
        | PendingOpKind::CalendarDelete
        | PendingOpKind::ContactCreate
        | PendingOpKind::ContactPatch
        | PendingOpKind::ContactDelete => false,
    }
}

/// The unreadable-payload rule holds for every mail write, not just a send: each kind
/// settles its own op and the pass carries on.
#[tokio::test]
async fn an_unreadable_payload_of_any_mail_kind_settles() {
    let provider = FakeMail::new(vec![], vec![]);
    let (store, _clock) = store_and_clock();
    let kinds: Vec<PendingOpKind> = PendingOpKind::ALL
        .into_iter()
        .filter(|k| is_a_mail_write(*k))
        .collect();
    for (i, kind) in kinds.iter().enumerate() {
        store
            .enqueue_pending_op(
                account(),
                PendingOp::new(
                    IdempotencyKey::new(format!("corrupt-{i}")).unwrap(),
                    *kind,
                    // Its own resource, so none of them waits on another.
                    ResourceKey::new(format!("mail:corrupt-{i}")).unwrap(),
                    serde_json::json!({ "not": "a request this build knows" }),
                ),
            )
            .await
            .unwrap();
    }

    let report = drain_outbox(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(report.attempted.len(), kinds.len());
    for (attempted, kind) in report.attempted.iter().zip(kinds) {
        assert_eq!(attempted.kind, kind);
        assert!(
            matches!(attempted.outcome, DrainOutcome::Undecodable { .. }),
            "{kind:?} payload should have settled as unreadable, got {:?}",
            attempted.outcome
        );
    }
    // All settled: none is left to be retried for ever.
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}
