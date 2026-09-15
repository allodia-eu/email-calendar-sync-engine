//! Mail-mutation outbox-driver tests: enqueue-before-side-effect, success
//! recording, conflict-without-blind-retry, distinct-key serialization, and durable
//! payload round-trip. Uses the shared fakes/helpers from the parent via `use super::*`.

use super::*;

/// The message an edit targets (an IMAP-shaped key).
fn target() -> ProviderKey {
    ProviderKey::new("imap:v1:u42@INBOX").unwrap()
}

#[tokio::test]
async fn edit_mail_enqueues_then_applies_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let outcome = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:seen:on",
        &MailEdit::mark_seen(target(), true),
    )
    .await
    .unwrap();

    // The edit resolved to the target message key and reached terminal success.
    assert_eq!(outcome.message_key, target());
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn edit_mail_records_conflict_without_blind_retry() {
    // A stale target (UIDVALIDITY changed) is recorded Failed (class Conflict) and
    // returned — the caller re-syncs and recomputes; the outbox does not blind-retry.
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::WriteGuard);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:delete",
        &MailEdit::delete(target()),
    )
    .await
    .unwrap_err();
    match err {
        crate::SyncError::Provider(e) => {
            assert_eq!(e.class(), engine_core::error::FailureClass::Conflict);
        }
        other => panic!("expected a provider error, got {other:?}"),
    }

    // Recover the op id via an idempotent re-enqueue; it was recorded Failed. The
    // resource key is `mail:{target}`, serializing edits to one message.
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:delete").unwrap(),
                ResourceKey::new("mail:imap:v1:u42@INBOX").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Failed)
    );
}

#[tokio::test]
async fn distinct_idempotency_keys_let_two_edits_of_one_message_both_run() {
    // The store dedups enqueue by (account, idempotency_key) across every op state,
    // so mark-read then mark-unread of ONE message must carry distinct keys to both
    // run — the reason the key is a caller-supplied argument.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let first = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:seen:on",
        &MailEdit::mark_seen(target(), true),
    )
    .await
    .unwrap();
    let second = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:seen:off",
        &MailEdit::mark_seen(target(), false),
    )
    .await
    .unwrap();

    assert_ne!(first.op, second.op);
    assert_eq!(
        store.pending_op_state(second.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[test]
fn edit_round_trips_through_a_durable_payload() {
    // The outbox stores the edit as a JSON payload; every variant must survive intact
    // for a recovery worker to re-apply it.
    let dest = MailboxId::try_from("Trash").unwrap();
    for original in [
        MailEdit::mark_seen(target(), true),
        MailEdit::set_flagged(target(), false),
        MailEdit::move_to(target(), dest),
        MailEdit::delete(target()),
    ] {
        let payload = serde_json::to_value(&original).unwrap();
        let restored: MailEdit = serde_json::from_value(payload).unwrap();
        assert_eq!(restored, original);
    }
}

/// A backlog of ops nothing has resolved must not starve a new edit.
///
/// The inline driver claims the op it just enqueued. Reaching that op through a
/// *batch* claim capped at N makes the outbox a queue with a head: once N older
/// runnable ops accumulate on an account, every subsequent write is refused, and
/// refusing it adds one more. That is unrecoverable without a drainer, so the
/// claim targets the op by id.
#[tokio::test]
async fn an_edit_applies_behind_a_backlog_of_unresolved_ops() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    // Ops on distinct resources that nothing ever resolves: what a host accumulates
    // today, one per interrupted write.
    for i in 0..40 {
        store
            .enqueue_pending_op(
                account(),
                PendingOp::new(
                    IdempotencyKey::new(format!("stuck:{i}")).unwrap(),
                    ResourceKey::new(format!("mail:imap:v1:u{i}@INBOX")).unwrap(),
                    serde_json::Value::Null,
                ),
            )
            .await
            .unwrap();
    }

    let outcome = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:seen:on",
        &MailEdit::mark_seen(target(), true),
    )
    .await
    .unwrap();

    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// Two edits of one message serialize; the second does not fail.
///
/// A resource collision is the outbox doing its job: an IMAP move invalidates the
/// UID a concurrent `STORE` names, so writes to one message run one at a time.
/// What the second write must not do is *give up* — a host marks a message read on
/// open and archives it a moment later, and the archive arrives inside the
/// mark-read's round trip.
#[tokio::test]
async fn a_second_edit_of_one_message_waits_for_the_first() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    // The mark-read, claimed and in flight: its lease holds `mail:{target}`.
    let blocking = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:seen:on").unwrap(),
                ResourceKey::new(format!("mail:{}", target().as_str())).unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    let leased = store
        .claim_pending_ops(
            account(),
            LeaseRequest::new(worker(), Duration::from_mins(5)),
            1,
        )
        .await
        .unwrap();
    assert_eq!(leased[0].id, blocking);

    // The archive arrives while that lease is live, and the mark-read lands shortly after.
    let archive = MailEdit::move_to(target(), MailboxId::try_from("Archive").unwrap());
    let acct = account();
    let (archived, ()) = tokio::join!(
        edit_mail(
            &provider,
            &store,
            &acct,
            worker(),
            Duration::from_mins(1),
            "edit:u42:archive",
            &archive,
        ),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            store
                .mark_pending_op(
                    &leased[0].lease,
                    PendingOutcome::Succeeded {
                        provider_key: target(),
                    },
                )
                .await
                .unwrap();
        }
    );

    let outcome = archived.expect("the archive waits for the mark-read, it does not fail");
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// A resource nothing releases fails the write once, and leaves the op behind.
///
/// The wait is bounded: a lease leaked by a process that died mid-write would
/// otherwise hold the caller for the lease's whole TTL. Past the bound the caller is
/// told which condition refused it, and the op stays durably enqueued — the intent is
/// the drainer's to finish, not this driver's to discard.
#[tokio::test(start_paused = true)]
async fn a_resource_nothing_releases_fails_the_write_after_the_bound() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:seen:on").unwrap(),
                ResourceKey::new(format!("mail:{}", target().as_str())).unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    store
        .claim_pending_ops(
            account(),
            LeaseRequest::new(worker(), Duration::from_mins(5)),
            1,
        )
        .await
        .unwrap();

    let err = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:archive",
        &MailEdit::move_to(target(), MailboxId::try_from("Archive").unwrap()),
    )
    .await
    .unwrap_err();
    let crate::SyncError::Outbox(message) = err else {
        panic!("a held resource is an outbox refusal, got {err:?}")
    };
    assert!(
        message.contains("Busy"),
        "the refusal must name its condition: {message}"
    );

    // Still enqueued, still runnable once the holder settles.
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:archive").unwrap(),
                ResourceKey::new(format!("mail:{}", target().as_str())).unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Pending)
    );
}
